//! The disk usage analyzer's model: one walk that measures every folder under
//! a root, so the view can rank the root's children at once and drilling into
//! a folder of the analysis costs nothing.
//!
//! The accounting rules are those of [`crate::core::du`] (and of
//! docs/roadmap.md): allocated blocks (`st_blocks * 512`), each
//! `(device, inode)` pair once across the whole analysis (the first name the
//! walk meets carries the bytes, later names of the same inode count as
//! items with no bytes), symbolic links never followed, one filesystem, a
//! folder that cannot be read flagged incomplete. The walk is blocking, so
//! callers run it off the UI thread; it checks a cancel flag per entry and
//! streams the running per-child totals through [`UsageProgress`].
//!
//! Memory is bounded: every folder is kept, but a folder keeps only its
//! [`MAX_FILES_PER_FOLDER`] largest files individually; the rest are folded
//! into one "other files" bucket that still counts toward every total.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

/// The device a folder is on, from its path and its own (not followed)
/// metadata. Injectable so the device boundary is testable on one device.
pub type DeviceOf = fn(&Path, &Metadata) -> u64;

fn metadata_device(_path: &Path, meta: &Metadata) -> u64 {
    meta.dev()
}

/// How many files one folder keeps individually; the others are folded into
/// its "other files" bucket.
pub const MAX_FILES_PER_FOLDER: usize = 200;

/// A folder of the analysis; the root is [`UsageTree::ROOT`].
pub type NodeId = usize;

/// One child of a folder: a subfolder, a kept file or the folded bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Folder(NodeId),
    /// `index` into the folder's kept files (largest first); valid until the
    /// next [`UsageTree::remove`].
    File {
        dir: NodeId,
        index: usize,
    },
    /// The files of `dir` that are not kept individually.
    OtherFiles(NodeId),
}

/// One ranked row: what the list and the status line show.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub item: Item,
    pub name: OsString,
    /// The path of the item; of the folder itself for the bucket.
    pub path: PathBuf,
    /// Allocated bytes, hard links counted once.
    pub bytes: u64,
    /// Entries below a folder (files and folders), the file count of the
    /// bucket, 0 for a file.
    pub items: u64,
    /// Something below could not be read, so the size is a lower bound.
    pub incomplete: bool,
    /// `bytes` over the total of the folder being ranked, 0 to 1.
    pub share: f32,
}

impl Row {
    #[must_use]
    pub fn is_folder(&self) -> bool {
        matches!(self.item, Item::Folder(_))
    }

    /// A row that names a real path (the bucket does not).
    #[must_use]
    pub fn is_real(&self) -> bool {
        !matches!(self.item, Item::OtherFiles(_))
    }
}

#[derive(Clone, Debug)]
struct FileRow {
    name: OsString,
    bytes: u64,
}

#[derive(Clone, Debug)]
struct Node {
    name: OsString,
    parent: Option<NodeId>,
    /// The folder's own block.
    own: u64,
    /// Own block plus everything below.
    total: u64,
    /// Entries below (files and folders).
    items: u64,
    incomplete: bool,
    folders: Vec<NodeId>,
    files: Vec<FileRow>,
    other_count: u64,
    other_bytes: u64,
}

impl Node {
    fn new(name: OsString, parent: Option<NodeId>, own: u64) -> Self {
        Self {
            name,
            parent,
            own,
            total: own,
            items: 0,
            incomplete: false,
            folders: Vec::new(),
            files: Vec::new(),
            other_count: 0,
            other_bytes: 0,
        }
    }
}

/// What [`UsageTree::remove`] took out of the totals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removed {
    pub bytes: u64,
    /// The item itself and everything below it.
    pub items: u64,
}

/// The finished analysis of a root.
#[derive(Clone, Debug)]
pub struct UsageTree {
    root: PathBuf,
    nodes: Vec<Node>,
    estimate: bool,
    /// What an id outside `nodes` reads as.
    missing: Node,
}

impl UsageTree {
    pub const ROOT: NodeId = 0;

    /// The folder `id`; an id that is not in the tree (never handed out by
    /// it) reads as an empty folder.
    fn node(&self, id: NodeId) -> &Node {
        self.nodes.get(id).unwrap_or(&self.missing)
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.nodes.get_mut(id).unwrap_or(&mut self.missing)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Own block plus everything below `dir`.
    #[must_use]
    pub fn total(&self, dir: NodeId) -> u64 {
        self.node(dir).total
    }

    /// The folder's own block, which no child row carries.
    #[must_use]
    pub fn own_bytes(&self, dir: NodeId) -> u64 {
        self.node(dir).own
    }

    /// Entries below `dir`.
    #[must_use]
    pub fn item_count(&self, dir: NodeId) -> u64 {
        self.node(dir).items
    }

    #[must_use]
    pub fn is_incomplete(&self, dir: NodeId) -> bool {
        self.node(dir).incomplete
    }

    /// The totals were adjusted after a removal, not measured again.
    #[must_use]
    pub fn is_estimate(&self) -> bool {
        self.estimate
    }

    #[must_use]
    pub fn path_of(&self, dir: NodeId) -> PathBuf {
        let mut names = Vec::new();
        let mut at = dir;
        while let Some(parent) = self.node(at).parent {
            names.push(&self.node(at).name);
            at = parent;
        }
        let mut path = self.root.clone();
        path.extend(names.into_iter().rev());
        path
    }

    /// The folder of the analysis at `path`, when `path` is inside it.
    #[must_use]
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let mut at = Self::ROOT;
        for part in relative.components() {
            let name = part.as_os_str();
            at = *self
                .node(at)
                .folders
                .iter()
                .find(|&&child| self.node(child).name == name)?;
        }
        Some(at)
    }

    /// The children of `dir` with their bytes, largest first (ties by name),
    /// without the empty ones.
    #[must_use]
    pub fn items(&self, dir: NodeId) -> Vec<(Item, u64)> {
        let node = self.node(dir);
        let mut items: Vec<(Item, u64)> = node
            .folders
            .iter()
            .map(|&child| (Item::Folder(child), self.node(child).total))
            .collect();
        items.extend(
            node.files
                .iter()
                .enumerate()
                .map(|(index, file)| (Item::File { dir, index }, file.bytes)),
        );
        if node.other_count > 0 {
            items.push((Item::OtherFiles(dir), node.other_bytes));
        }
        items.sort_by(|(a, a_bytes), (b, b_bytes)| {
            b_bytes
                .cmp(a_bytes)
                .then_with(|| self.item_name(*a).cmp(self.item_name(*b)))
        });
        items
    }

    fn item_name(&self, item: Item) -> &OsStr {
        match item {
            Item::Folder(id) => &self.node(id).name,
            Item::File { dir, index } => self
                .node(dir)
                .files
                .get(index)
                .map_or(OsStr::new(""), |file| &file.name),
            Item::OtherFiles(_) => OsStr::new(""),
        }
    }

    /// The row of one item, ranked against `dir`'s total.
    #[must_use]
    pub fn row(&self, item: Item, bytes: u64, share_of: u64) -> Row {
        let share = if share_of == 0 {
            0.0
        } else {
            (bytes as f64 / share_of as f64).min(1.0) as f32
        };
        match item {
            Item::Folder(id) => Row {
                item,
                name: self.node(id).name.clone(),
                path: self.path_of(id),
                bytes,
                items: self.node(id).items,
                incomplete: self.node(id).incomplete,
                share,
            },
            Item::File { dir, index } => {
                let name = self
                    .node(dir)
                    .files
                    .get(index)
                    .map(|file| file.name.clone())
                    .unwrap_or_default();
                Row {
                    item,
                    path: self.path_of(dir).join(&name),
                    name,
                    bytes,
                    items: 0,
                    incomplete: false,
                    share,
                }
            }
            Item::OtherFiles(dir) => Row {
                item,
                name: OsString::from(format!("{} other files", self.node(dir).other_count)),
                path: self.path_of(dir),
                bytes,
                items: self.node(dir).other_count,
                incomplete: false,
                share,
            },
        }
    }

    /// The ranked rows of `dir`: largest first, each with its share of the
    /// total of `dir`.
    #[must_use]
    pub fn rows(&self, dir: NodeId) -> Vec<Row> {
        let total = self.total(dir);
        self.items(dir)
            .into_iter()
            .map(|(item, bytes)| self.row(item, bytes, total))
            .collect()
    }

    /// Takes the item at `path` out of the totals of every folder above it
    /// (after it was removed from disk) and marks the tree as an estimate:
    /// the bytes subtracted are the ones measured, which may differ from what
    /// the removal freed. `None` when `path` is not an item of the analysis
    /// (the root itself, a file folded into a bucket, another tree).
    pub fn remove(&mut self, path: &Path) -> Option<Removed> {
        let parent = self.find(path.parent()?)?;
        let name = path.file_name()?;
        let folder_at = self
            .node(parent)
            .folders
            .iter()
            .position(|&child| self.node(child).name == name);
        let removed = if let Some(at) = folder_at {
            let child = self.node_mut(parent).folders.remove(at);
            Removed {
                bytes: self.node(child).total,
                items: self.node(child).items + 1,
            }
        } else {
            let at = self
                .node(parent)
                .files
                .iter()
                .position(|file| file.name == name)?;
            let file = self.node_mut(parent).files.remove(at);
            Removed {
                bytes: file.bytes,
                items: 1,
            }
        };
        let mut at = Some(parent);
        while let Some(id) = at {
            let node = self.node_mut(id);
            node.total = node.total.saturating_sub(removed.bytes);
            node.items = node.items.saturating_sub(removed.items);
            at = node.parent;
        }
        self.estimate = true;
        Some(removed)
    }
}

/// One child of the root while the walk runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialRow {
    pub name: OsString,
    pub is_folder: bool,
    pub bytes: u64,
    pub items: u64,
    pub incomplete: bool,
    /// The walk of this child ended: the numbers are final.
    pub done: bool,
}

/// The per-child totals counted so far, for the streamed view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Partial {
    /// The root's folders and its kept files, in the order found.
    pub rows: Vec<PartialRow>,
    /// The root's files that are not listed individually.
    pub other_files: u64,
    pub other_bytes: u64,
    /// The root's own block plus every child counted so far.
    pub bytes: u64,
    pub items: u64,
    pub incomplete: bool,
}

impl Partial {
    /// The rows largest first, as the final ranking will order them.
    #[must_use]
    pub fn ranked(&self) -> Vec<PartialRow> {
        let mut rows = self.rows.clone();
        rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        rows
    }
}

/// Running totals shared with the thread that shows them.
#[derive(Debug, Default)]
pub struct UsageProgress {
    partial: Mutex<Partial>,
    finished: AtomicBool,
}

impl UsageProgress {
    /// The totals counted so far.
    #[must_use]
    pub fn snapshot(&self) -> Partial {
        self.partial
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the walk has returned (done, cancelled or failed).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    fn update(&self, change: impl FnOnce(&mut Partial)) {
        change(&mut self.partial.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// How a walk ended.
#[derive(Debug)]
pub enum UsageEnd {
    Done(UsageTree),
    Cancelled,
}

/// Analyzes `root`, publishing the per-child totals to `progress`. Fails only
/// when `root` itself cannot be inspected; everything below it degrades to
/// an incomplete flag.
pub fn analyze(root: &Path, cancel: &AtomicBool, progress: &UsageProgress) -> io::Result<UsageEnd> {
    analyze_with_device(root, metadata_device, cancel, progress)
}

/// [`analyze`] with another way to tell which device a folder is on.
pub fn analyze_with_device(
    root: &Path,
    device_of: DeviceOf,
    cancel: &AtomicBool,
    progress: &UsageProgress,
) -> io::Result<UsageEnd> {
    let result = Walk::new(root, device_of, cancel, progress).run();
    progress.finished.store(true, Ordering::Release);
    result
}

struct Walk<'a> {
    root: &'a Path,
    device_of: DeviceOf,
    cancel: &'a AtomicBool,
    progress: &'a UsageProgress,
    nodes: Vec<Node>,
    seen: HashSet<(u64, u64)>,
    device: u64,
    /// What an id outside `nodes` reads as (never handed out by the walk).
    scratch: Node,
}

/// What reading one folder added.
#[derive(Default)]
struct Scanned {
    dirs: Vec<(NodeId, PathBuf)>,
    bytes: u64,
    items: u64,
    incomplete: bool,
}

impl<'a> Walk<'a> {
    fn new(
        root: &'a Path,
        device_of: DeviceOf,
        cancel: &'a AtomicBool,
        progress: &'a UsageProgress,
    ) -> Self {
        Self {
            root,
            device_of,
            cancel,
            progress,
            nodes: Vec::new(),
            seen: HashSet::new(),
            device: 0,
            scratch: Node::new(OsString::new(), None, 0),
        }
    }

    fn node(&self, id: NodeId) -> &Node {
        self.nodes.get(id).unwrap_or(&self.scratch)
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.nodes.get_mut(id).unwrap_or(&mut self.scratch)
    }

    fn run(mut self) -> io::Result<UsageEnd> {
        let meta = std::fs::symlink_metadata(self.root)?;
        self.device = (self.device_of)(self.root, &meta);
        let name = self
            .root
            .file_name()
            .map_or_else(|| self.root.as_os_str().to_owned(), OsStr::to_owned);
        let own = meta.blocks().saturating_mul(512);
        self.nodes.push(Node::new(name, None, own));
        self.progress.update(|partial| partial.bytes = own);
        if meta.is_dir() {
            let Some(top) = self.scan(UsageTree::ROOT, self.root) else {
                return Ok(UsageEnd::Cancelled);
            };
            self.publish_top(&top);
            for (id, path) in top.dirs {
                if !self.walk_child(id, path) {
                    return Ok(UsageEnd::Cancelled);
                }
            }
        }
        self.aggregate();
        Ok(UsageEnd::Done(UsageTree {
            root: self.root.to_path_buf(),
            nodes: self.nodes,
            estimate: false,
            missing: Node::new(OsString::new(), None, 0),
        }))
    }

    /// The root's own row of the stream: its folders (not counted yet) and
    /// its kept files.
    fn publish_top(&self, top: &Scanned) {
        let root = self.node(UsageTree::ROOT);
        let mut rows: Vec<PartialRow> = root
            .files
            .iter()
            .map(|file| PartialRow {
                name: file.name.clone(),
                is_folder: false,
                bytes: file.bytes,
                items: 0,
                incomplete: false,
                done: true,
            })
            .collect();
        rows.extend(top.dirs.iter().map(|(id, _)| PartialRow {
            name: self.node(*id).name.clone(),
            is_folder: true,
            // The folder's own block is already in the level's bytes.
            bytes: self.node(*id).own,
            items: 0,
            incomplete: false,
            done: false,
        }));
        let (other, other_bytes) = (root.other_count, root.other_bytes);
        let total = root.own + top.bytes;
        let (items, incomplete) = (top.items, top.incomplete);
        self.progress.update(|partial| {
            partial.rows = rows;
            partial.other_files = other;
            partial.other_bytes = other_bytes;
            partial.bytes = total;
            partial.items = items;
            partial.incomplete = incomplete;
        });
    }

    /// Walks the whole subtree of one top-level folder, publishing its
    /// running totals after every folder read. `false` when cancelled.
    fn walk_child(&mut self, id: NodeId, path: PathBuf) -> bool {
        let name = self.node(id).name.clone();
        let own = self.node(id).own;
        let (mut bytes, mut items, mut incomplete) = (own, 0u64, false);
        let mut stack = vec![(id, path)];
        self.publish_child(&name, (bytes, items, incomplete), false);
        while let Some((dir, path)) = stack.pop() {
            let Some(scanned) = self.scan(dir, &path) else {
                return false;
            };
            bytes += scanned.bytes;
            items += scanned.items;
            incomplete |= scanned.incomplete;
            stack.extend(scanned.dirs);
            self.publish_child(&name, (bytes, items, incomplete), stack.is_empty());
        }
        true
    }

    fn publish_child(
        &self,
        name: &OsStr,
        (bytes, items, incomplete): (u64, u64, bool),
        done: bool,
    ) {
        self.progress.update(|partial| {
            let Some(row) = partial
                .rows
                .iter_mut()
                .find(|row| row.is_folder && row.name == name)
            else {
                return;
            };
            let before = (row.bytes, row.items);
            row.bytes = bytes;
            row.items = items;
            row.incomplete = incomplete;
            row.done = done;
            partial.bytes = partial.bytes.saturating_add(bytes.saturating_sub(before.0));
            partial.items = partial.items.saturating_add(items.saturating_sub(before.1));
            partial.incomplete |= incomplete;
        });
    }

    /// Reads one folder: registers its subfolders as nodes, keeps its
    /// largest files and folds the rest. `None` when cancelled.
    fn scan(&mut self, dir: NodeId, path: &Path) -> Option<Scanned> {
        let mut scanned = Scanned::default();
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                log::debug!("usage of {}: skipped, {error}", path.display());
                self.node_mut(dir).incomplete = true;
                scanned.incomplete = true;
                return Some(scanned);
            }
        };
        let mut files: Vec<FileRow> = Vec::new();
        for entry in entries {
            if self.cancel.load(Ordering::Relaxed) {
                return None;
            }
            // `DirEntry::metadata` is an lstat: a link is never followed.
            let (entry_path, meta, name) = match entry.and_then(|entry| {
                let meta = entry.metadata()?;
                Ok((entry.path(), meta, entry.file_name()))
            }) {
                Ok(found) => found,
                Err(error) => {
                    log::debug!("usage of {}: entry skipped, {error}", path.display());
                    self.node_mut(dir).incomplete = true;
                    scanned.incomplete = true;
                    continue;
                }
            };
            if (self.device_of)(&entry_path, &meta) != self.device {
                continue; // a mount below the folder belongs to another volume
            }
            let bytes = self.take_blocks(&meta);
            scanned.bytes += bytes;
            scanned.items += 1;
            if meta.is_dir() {
                let child = self.nodes.len();
                self.nodes.push(Node::new(name, Some(dir), bytes));
                self.node_mut(dir).folders.push(child);
                scanned.dirs.push((child, entry_path));
            } else {
                files.push(FileRow { name, bytes });
            }
        }
        files.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        if files.len() > MAX_FILES_PER_FOLDER {
            let folded = files.split_off(MAX_FILES_PER_FOLDER);
            let node = self.node_mut(dir);
            node.other_count = folded.len() as u64;
            node.other_bytes = folded.iter().map(|file| file.bytes).sum();
        }
        self.node_mut(dir).files = files;
        Some(scanned)
    }

    /// The allocated bytes of one inode, once per `(device, inode)`.
    fn take_blocks(&mut self, meta: &Metadata) -> u64 {
        // Only multiply-linked files can repeat; the set stays small.
        if meta.nlink() > 1 && !meta.is_dir() && !self.seen.insert((meta.dev(), meta.ino())) {
            return 0;
        }
        meta.blocks().saturating_mul(512)
    }

    /// Rolls every folder's numbers up into its parent. A child always comes
    /// after its parent in the arena, so one pass from the end suffices.
    fn aggregate(&mut self) {
        for node in &mut self.nodes {
            let direct_files: u64 = node.files.iter().map(|file| file.bytes).sum();
            node.total = node.own + direct_files + node.other_bytes;
            node.items = node.files.len() as u64 + node.other_count + node.folders.len() as u64;
        }
        for id in (1..self.nodes.len()).rev() {
            let (total, items, incomplete, parent) = {
                let node = self.node(id);
                (node.total, node.items, node.incomplete, node.parent)
            };
            if let Some(parent) = parent {
                let parent = self.node_mut(parent);
                parent.total += total;
                parent.items += items;
                parent.incomplete |= incomplete;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::core::du;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct Scratch(PathBuf);

    impl Scratch {
        pub(crate) fn new(tag: &str) -> io::Result<Self> {
            let root = std::env::temp_dir().join(format!(
                "fileflux-usage-test-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root)?;
            Ok(Self(root))
        }
        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Ok(entries) = fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755)).ok();
                }
            }
            fs::remove_dir_all(&self.0).ok();
        }
    }

    pub(crate) fn run(root: &Path) -> io::Result<UsageTree> {
        match analyze(root, &AtomicBool::new(false), &UsageProgress::default())? {
            UsageEnd::Done(tree) => Ok(tree),
            UsageEnd::Cancelled => Err(io::Error::other("cancelled")),
        }
    }

    fn blocks(path: &Path) -> io::Result<u64> {
        Ok(fs::symlink_metadata(path)?.blocks() * 512)
    }

    fn walked(path: &Path) -> io::Result<du::DiskUsage> {
        match du::walk(path, &AtomicBool::new(false), &du::Progress::default())? {
            du::WalkEnd::Done(usage) => Ok(usage),
            du::WalkEnd::Cancelled => Err(io::Error::other("cancelled")),
        }
    }

    fn named<'a>(rows: &'a [Row], name: &str) -> Option<&'a Row> {
        rows.iter().find(|row| row.name == name)
    }

    fn sample(root: &Path) -> io::Result<()> {
        fs::create_dir_all(root.join("a/deep"))?;
        fs::create_dir_all(root.join("b"))?;
        fs::write(root.join("top.bin"), vec![1u8; 30_000])?;
        fs::write(root.join("a/one.bin"), vec![1u8; 200_000])?;
        fs::write(root.join("a/deep/two.bin"), vec![2u8; 60_000])?;
        fs::write(root.join("b/three.bin"), vec![3u8; 90_000])?;
        Ok(())
    }

    #[test]
    fn the_children_sum_to_the_root_and_match_the_single_folder_walk() -> io::Result<()> {
        let scratch = Scratch::new("sum")?;
        sample(scratch.path())?;
        let tree = run(scratch.path())?;
        let rows = tree.rows(UsageTree::ROOT);
        let children: u64 = rows.iter().map(|row| row.bytes).sum();
        assert_eq!(
            children + tree.own_bytes(UsageTree::ROOT),
            tree.total(UsageTree::ROOT)
        );
        let walk = walked(scratch.path())?;
        assert_eq!(tree.total(UsageTree::ROOT), walk.bytes);
        assert_eq!(tree.item_count(UsageTree::ROOT), walk.files + walk.folders);
        let a = named(&rows, "a").ok_or_else(|| io::Error::other("no row a"))?;
        assert_eq!(a.bytes, walked(&scratch.path().join("a"))?.bytes);
        assert_eq!(a.items, 3, "one.bin, deep and deep/two.bin");
        assert!(a.is_folder() && !a.incomplete);
        let top = named(&rows, "top.bin").ok_or_else(|| io::Error::other("no row top.bin"))?;
        assert_eq!(
            (top.bytes, top.items),
            (blocks(&scratch.path().join("top.bin"))?, 0)
        );
        Ok(())
    }

    #[test]
    fn rows_are_ranked_largest_first_with_their_share_of_the_folder() -> io::Result<()> {
        let scratch = Scratch::new("rank")?;
        sample(scratch.path())?;
        let tree = run(scratch.path())?;
        let rows = tree.rows(UsageTree::ROOT);
        let sizes: Vec<u64> = rows.iter().map(|row| row.bytes).collect();
        let mut sorted = sizes.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted);
        assert_eq!(rows[0].name, "a", "a holds 260 kB, the rest less");
        let total = tree.total(UsageTree::ROOT) as f64;
        for row in &rows {
            assert!((f64::from(row.share) - row.bytes as f64 / total).abs() < 1e-6);
            assert!(row.share > 0.0 && row.share <= 1.0);
        }
        let share_sum: f32 = rows.iter().map(|row| row.share).sum();
        assert!(share_sum <= 1.0 + 1e-6);
        Ok(())
    }

    #[test]
    fn a_hard_link_counts_once_across_children() -> io::Result<()> {
        let scratch = Scratch::new("hard")?;
        let root = scratch.path();
        fs::create_dir_all(root.join("a"))?;
        fs::create_dir_all(root.join("b"))?;
        fs::write(root.join("a/orig"), vec![7u8; 80_000])?;
        fs::hard_link(root.join("a/orig"), root.join("b/twin"))?;
        fs::hard_link(root.join("a/orig"), root.join("third"))?;
        let tree = run(root)?;
        let expected = blocks(root)?
            + blocks(&root.join("a"))?
            + blocks(&root.join("b"))?
            + blocks(&root.join("a/orig"))?;
        assert_eq!(tree.total(UsageTree::ROOT), expected);
        assert_eq!(tree.total(UsageTree::ROOT), walked(root)?.bytes);
        let rows = tree.rows(UsageTree::ROOT);
        let carriers = rows
            .iter()
            .filter(|row| row.is_folder() && row.bytes > blocks(&root.join("a")).unwrap_or(0))
            .count()
            + rows
                .iter()
                .filter(|row| !row.is_folder() && row.bytes > 0)
                .count();
        assert_eq!(carriers, 1, "exactly one name carries the inode's bytes");
        assert_eq!(
            tree.item_count(UsageTree::ROOT),
            5,
            "every name is still an item"
        );
        Ok(())
    }

    #[test]
    fn symlinks_are_not_followed() -> io::Result<()> {
        let scratch = Scratch::new("links")?;
        let outside = Scratch::new("links-outside")?;
        fs::write(outside.path().join("big"), vec![3u8; 500_000])?;
        fs::create_dir(outside.path().join("dir"))?;
        fs::write(outside.path().join("dir/inner"), vec![3u8; 500_000])?;
        let root = scratch.path();
        symlink(outside.path().join("big"), root.join("to-file"))?;
        symlink(outside.path().join("dir"), root.join("to-dir"))?;
        symlink(root, root.join("loop"))?;
        let tree = run(root)?;
        assert_eq!(
            tree.total(UsageTree::ROOT),
            blocks(root)?
                + blocks(&root.join("to-file"))?
                + blocks(&root.join("to-dir"))?
                + blocks(&root.join("loop"))?
        );
        let rows = tree.rows(UsageTree::ROOT);
        assert!(
            rows.iter().all(|row| !row.is_folder()),
            "a link is not a folder"
        );
        assert_eq!(tree.find(&root.join("to-dir")), None);
        Ok(())
    }

    fn other_device(path: &Path, meta: &Metadata) -> u64 {
        if path.file_name().is_some_and(|name| name == "mnt") {
            meta.dev() + 1
        } else {
            meta.dev()
        }
    }

    #[test]
    fn a_folder_on_another_device_is_skipped() -> io::Result<()> {
        let scratch = Scratch::new("device")?;
        let root = scratch.path();
        fs::create_dir_all(root.join("mnt"))?;
        fs::write(root.join("mnt/foreign"), vec![1u8; 100_000])?;
        fs::create_dir_all(root.join("home"))?;
        fs::write(root.join("home/mine"), vec![1u8; 10_000])?;
        let tree = match analyze_with_device(
            root,
            other_device,
            &AtomicBool::new(false),
            &UsageProgress::default(),
        )? {
            UsageEnd::Done(tree) => tree,
            UsageEnd::Cancelled => return Err(io::Error::other("cancelled")),
        };
        let rows = tree.rows(UsageTree::ROOT);
        assert!(named(&rows, "mnt").is_none(), "the mount is not a row");
        assert_eq!(
            tree.total(UsageTree::ROOT),
            blocks(root)? + blocks(&root.join("home"))? + blocks(&root.join("home/mine"))?
        );
        Ok(())
    }

    #[test]
    fn an_unreadable_folder_flags_only_its_branch_and_the_ancestors() -> io::Result<()> {
        let scratch = Scratch::new("denied")?;
        let root = scratch.path();
        fs::create_dir_all(root.join("ok"))?;
        fs::create_dir_all(root.join("wrap/locked"))?;
        fs::write(root.join("ok/f"), vec![1u8; 20_000])?;
        fs::set_permissions(root.join("wrap/locked"), fs::Permissions::from_mode(0o000))?;
        if fs::read_dir(root.join("wrap/locked")).is_ok() {
            return Ok(()); // running as root: nothing is unreadable
        }
        let tree = run(root)?;
        let rows = tree.rows(UsageTree::ROOT);
        assert!(named(&rows, "wrap").is_some_and(|row| row.incomplete));
        assert!(named(&rows, "ok").is_some_and(|row| !row.incomplete));
        assert!(tree.is_incomplete(UsageTree::ROOT));
        Ok(())
    }

    #[test]
    fn a_raised_cancel_flag_ends_the_walk_without_a_tree() -> io::Result<()> {
        let scratch = Scratch::new("cancel")?;
        sample(scratch.path())?;
        let progress = UsageProgress::default();
        let end = analyze(scratch.path(), &AtomicBool::new(true), &progress)?;
        assert!(matches!(end, UsageEnd::Cancelled));
        assert!(progress.is_finished());
        Ok(())
    }

    #[test]
    fn a_missing_root_is_an_error() -> io::Result<()> {
        let scratch = Scratch::new("missing")?;
        let progress = UsageProgress::default();
        let error = analyze(
            &scratch.path().join("nope"),
            &AtomicBool::new(false),
            &progress,
        )
        .err()
        .map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::NotFound));
        assert!(progress.is_finished());
        Ok(())
    }

    #[test]
    fn the_stream_ends_with_the_final_per_child_totals() -> io::Result<()> {
        let scratch = Scratch::new("stream")?;
        sample(scratch.path())?;
        let progress = UsageProgress::default();
        let end = analyze(scratch.path(), &AtomicBool::new(false), &progress)?;
        let UsageEnd::Done(tree) = end else {
            return Err(io::Error::other("cancelled"));
        };
        let partial = progress.snapshot();
        assert_eq!(partial.bytes, tree.total(UsageTree::ROOT));
        assert_eq!(partial.items, tree.item_count(UsageTree::ROOT));
        assert!(partial.rows.iter().all(|row| row.done));
        for row in tree.rows(UsageTree::ROOT) {
            let streamed = partial
                .rows
                .iter()
                .find(|candidate| candidate.name == row.name)
                .ok_or_else(|| io::Error::other("row not streamed"))?;
            assert_eq!((streamed.bytes, streamed.items), (row.bytes, row.items));
        }
        let ranked: Vec<u64> = partial.ranked().iter().map(|row| row.bytes).collect();
        assert!(ranked.windows(2).all(|pair| pair[0] >= pair[1]));
        Ok(())
    }

    #[test]
    fn a_folder_inside_the_analysis_is_found_with_its_totals() -> io::Result<()> {
        let scratch = Scratch::new("cache")?;
        sample(scratch.path())?;
        let tree = run(scratch.path())?;
        let deep = scratch.path().join("a/deep");
        let id = tree
            .find(&deep)
            .ok_or_else(|| io::Error::other("deep not cached"))?;
        assert_eq!(tree.total(id), walked(&deep)?.bytes);
        assert_eq!(tree.path_of(id), deep);
        let rows = tree.rows(id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, deep.join("two.bin"));
        assert!(
            (rows[0].share - 1.0).abs() < 0.2,
            "the file is nearly the whole folder"
        );
        assert_eq!(tree.find(scratch.path()), Some(UsageTree::ROOT));
        assert_eq!(tree.find(&scratch.path().join("a/nope")), None);
        assert_eq!(tree.find(Path::new("/elsewhere")), None);
        Ok(())
    }

    #[test]
    fn a_folder_keeps_its_largest_files_and_folds_the_rest_into_a_bucket() -> io::Result<()> {
        let scratch = Scratch::new("fold")?;
        let root = scratch.path();
        let extra = 7;
        for index in 0..MAX_FILES_PER_FOLDER + extra {
            fs::write(root.join(format!("f{index:04}")), vec![1u8; 1_000])?;
        }
        fs::write(root.join("huge"), vec![1u8; 300_000])?;
        let tree = run(root)?;
        let rows = tree.rows(UsageTree::ROOT);
        assert_eq!(rows[0].name, "huge");
        let bucket = rows
            .iter()
            .find(|row| !row.is_real())
            .ok_or_else(|| io::Error::other("no bucket"))?;
        assert_eq!(bucket.items, (extra + 1) as u64);
        assert_eq!(rows.len(), MAX_FILES_PER_FOLDER + 1);
        assert_eq!(tree.total(UsageTree::ROOT), walked(root)?.bytes);
        Ok(())
    }

    #[test]
    fn removing_an_item_subtracts_its_measured_size_from_every_ancestor() -> io::Result<()> {
        let scratch = Scratch::new("remove")?;
        sample(scratch.path())?;
        let mut tree = run(scratch.path())?;
        let root_total = tree.total(UsageTree::ROOT);
        let a = scratch.path().join("a");
        let a_id = tree.find(&a).ok_or_else(|| io::Error::other("a"))?;
        let a_total = tree.total(a_id);
        let a_items = tree.item_count(a_id);
        assert!(!tree.is_estimate());

        let deep = scratch.path().join("a/deep");
        let deep_total = tree.total(tree.find(&deep).ok_or_else(|| io::Error::other("deep"))?);
        let removed = tree
            .remove(&deep)
            .ok_or_else(|| io::Error::other("not removed"))?;
        assert_eq!(
            removed,
            Removed {
                bytes: deep_total,
                items: 2
            }
        );
        assert_eq!(tree.total(a_id), a_total - deep_total);
        assert_eq!(tree.total(UsageTree::ROOT), root_total - deep_total);
        assert_eq!(tree.item_count(a_id), a_items - 2);
        assert!(tree.is_estimate());
        assert!(tree.find(&deep).is_none());
        assert!(named(&tree.rows(a_id), "deep").is_none());

        let one = scratch.path().join("a/one.bin");
        let one_bytes = blocks(&one)?;
        let removed = tree
            .remove(&one)
            .ok_or_else(|| io::Error::other("file not removed"))?;
        assert_eq!(
            removed,
            Removed {
                bytes: one_bytes,
                items: 1
            }
        );
        assert_eq!(
            tree.total(UsageTree::ROOT),
            root_total - deep_total - one_bytes
        );
        Ok(())
    }

    #[test]
    fn removing_what_the_analysis_does_not_hold_changes_nothing() -> io::Result<()> {
        let scratch = Scratch::new("remove-none")?;
        sample(scratch.path())?;
        let mut tree = run(scratch.path())?;
        let before = tree.total(UsageTree::ROOT);
        assert_eq!(tree.remove(scratch.path()), None, "the root is not an item");
        assert_eq!(tree.remove(&scratch.path().join("nope")), None);
        assert_eq!(tree.remove(Path::new("/elsewhere/x")), None);
        assert_eq!(tree.total(UsageTree::ROOT), before);
        assert!(!tree.is_estimate());
        Ok(())
    }
}
