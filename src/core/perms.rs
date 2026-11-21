//! Editable permissions and group (the details panel's editor).
//!
//! The model follows the GNOME Files convention: per class (owner, group,
//! others) an access of none, read only or read and write; files add an
//! "allow executing" toggle, folders map the access to `r-x` / `rwx` so a
//! readable folder stays listable. The setuid, setgid and sticky bits are
//! never changed by an edit. Everything is pure except the `apply`/`restore`
//! functions, which are blocking (callers run them off the UI thread), never
//! follow a symbolic link (an entry is pinned with `O_PATH | O_NOFOLLOW` and
//! changed through that descriptor), and report what they changed with the
//! exact previous modes and groups so an undo can restore them.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// One of the three permission classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Owner,
    Group,
    Others,
}

impl Class {
    pub const ALL: [Class; 3] = [Class::Owner, Class::Group, Class::Others];

    fn shift(self) -> u32 {
        match self {
            Class::Owner => 6,
            Class::Group => 3,
            Class::Others => 0,
        }
    }

    /// The label of the class's row.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Class::Owner => "Owner",
            Class::Group => "Group",
            Class::Others => "Others",
        }
    }
}

/// What a class may do with an entry's contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    None,
    /// Write without read: shown when found, never offered as a choice.
    WriteOnly,
    Read,
    ReadWrite,
    /// A folder class that fits none of the choices (for example `r--`,
    /// which cannot be entered): shown as found, never offered, and never
    /// equal to a choice, so picking any choice changes it.
    Custom,
}

impl Access {
    /// The three choices the editor offers.
    pub const CHOICES: [Access; 3] = [Access::None, Access::Read, Access::ReadWrite];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Access::None => "None",
            Access::WriteOnly => "Write only",
            Access::Read => "Read only",
            Access::ReadWrite => "Read and write",
            Access::Custom => "Custom",
        }
    }

    fn can_read(self) -> bool {
        matches!(self, Access::Read | Access::ReadWrite)
    }
}

/// The access a class has in `mode` (the execute bit is not part of it).
#[must_use]
pub fn access_of(mode: u32, class: Class) -> Access {
    let bits = mode >> class.shift();
    match (bits & 0b100 != 0, bits & 0b010 != 0) {
        (true, true) => Access::ReadWrite,
        (true, false) => Access::Read,
        (false, true) => Access::WriteOnly,
        (false, false) => Access::None,
    }
}

/// The access a class has in `mode` of a file or a folder. A folder's
/// choices carry the x bit (`---`, `r-x`, `rwx`): any other combination with
/// read, write or x is [`Access::Custom`], not the choice it resembles.
#[must_use]
pub fn entry_access(mode: u32, is_dir: bool, class: Class) -> Access {
    let access = access_of(mode, class);
    if !is_dir {
        return access;
    }
    let execute = mode >> class.shift() & 0b001 != 0;
    match (access, execute) {
        (Access::None, false) | (Access::Read | Access::ReadWrite, true) => access,
        _ => Access::Custom,
    }
}

/// Whether a file is executable: by every class that can read it, or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exec {
    Off,
    On,
    /// Some execute bits are set, but not exactly for the readers.
    Partial,
}

#[must_use]
pub fn exec_of(mode: u32) -> Exec {
    let mut any = false;
    let mut exact = true;
    for class in Class::ALL {
        let executable = mode >> class.shift() & 0b001 != 0;
        any |= executable;
        exact &= executable == access_of(mode, class).can_read();
    }
    match (any, exact) {
        (false, _) => Exec::Off,
        (true, true) => Exec::On,
        (true, false) => Exec::Partial,
    }
}

/// What the user changed. A `None` field is untouched: that class keeps its
/// bits exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    pub owner: Option<Access>,
    pub group: Option<Access>,
    pub others: Option<Access>,
    /// "Allow executing", for files only (folders ignore it).
    pub exec: Option<bool>,
}

impl Edit {
    #[must_use]
    pub fn get(&self, class: Class) -> Option<Access> {
        match class {
            Class::Owner => self.owner,
            Class::Group => self.group,
            Class::Others => self.others,
        }
    }

    pub fn set(&mut self, class: Class, access: Option<Access>) {
        match class {
            Class::Owner => self.owner = access,
            Class::Group => self.group = access,
            Class::Others => self.others = access,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Edit::default()
    }

    /// Only what differs from the values every entry already shares: a
    /// choice equal to a shared value changes nothing and is dropped; a
    /// choice over mixed values is kept.
    #[must_use]
    pub fn changed_against(&self, summary: &Summary) -> Edit {
        let mut changed = *self;
        for class in Class::ALL {
            if self
                .get(class)
                .is_some_and(|access| summary.access(class) == Some(Merged::Same(access)))
            {
                changed.set(class, None);
            }
        }
        let already = match summary.exec {
            Some(Merged::Same(Exec::On)) => Some(true),
            Some(Merged::Same(Exec::Off)) => Some(false),
            _ => None,
        };
        if already.is_some() && already == self.exec {
            changed.exec = None;
        }
        changed
    }

    /// The rule "Apply to enclosed files" applies: every class the editor
    /// shows a value for (the user's choice, else the value all entries
    /// share); classes that are mixed and untouched stay out.
    #[must_use]
    pub fn effective(&self, summary: &Summary) -> Edit {
        let mut rule = Edit {
            exec: self.exec,
            ..Edit::default()
        };
        for class in Class::ALL {
            let shared = match summary.access(class) {
                Some(Merged::Same(Access::Custom)) => None,
                Some(Merged::Same(access)) => Some(access),
                _ => None,
            };
            rule.set(class, self.get(class).or(shared));
        }
        rule
    }
}

/// The mode after `edit`, for a folder or a file. Bits outside `0o777`
/// (setuid, setgid, sticky, file type) are returned as they were.
#[must_use]
pub fn apply_mode(mode: u32, is_dir: bool, edit: &Edit) -> u32 {
    let mut perms = mode & 0o777;
    for class in Class::ALL {
        let Some(access) = edit.get(class) else {
            continue;
        };
        let shift = class.shift();
        let read_write = match access {
            Access::None => 0b000,
            Access::WriteOnly => 0b010,
            Access::Read => 0b100,
            Access::ReadWrite => 0b110,
            // Never a choice: the class keeps its bits.
            Access::Custom => continue,
        };
        // A folder needs x to be entered; a file keeps the x it had, unless
        // the class now has no access at all.
        let execute = if access == Access::None {
            0
        } else if is_dir {
            0b001
        } else {
            perms >> shift & 0b001
        };
        perms = perms & !(0b111 << shift) | (read_write | execute) << shift;
    }
    if !is_dir && let Some(allow) = edit.exec {
        for class in Class::ALL {
            let bit = 0b001 << class.shift();
            if !allow {
                perms &= !bit;
            } else if access_of(perms, class).can_read() {
                perms |= bit;
            }
        }
    }
    mode & !0o777 | perms
}

/// All values equal, or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merged<T> {
    Same(T),
    Mixed,
}

/// Merges values; `None` when there are none.
pub fn merge<T: PartialEq + Copy>(values: impl IntoIterator<Item = T>) -> Option<Merged<T>> {
    let mut values = values.into_iter();
    let first = values.next()?;
    Some(if values.all(|value| value == first) {
        Merged::Same(first)
    } else {
        Merged::Mixed
    })
}

/// What the stat of one entry says (never following a symbolic link).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryState {
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// `st_mode`, file type included.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The shared and mixed values over several entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub folders: usize,
    /// Symbolic links: skipped, their own mode cannot be changed.
    pub symlinks: usize,
    pub owner: Option<Merged<Access>>,
    pub group: Option<Merged<Access>>,
    pub others: Option<Merged<Access>>,
    /// Over the files only.
    pub exec: Option<Merged<Exec>>,
    pub uid: Option<Merged<u32>>,
    pub gid: Option<Merged<u32>>,
}

impl Summary {
    #[must_use]
    pub fn access(&self, class: Class) -> Option<Merged<Access>> {
        match class {
            Class::Owner => self.owner,
            Class::Group => self.group,
            Class::Others => self.others,
        }
    }
}

#[must_use]
pub fn summarize(states: &[EntryState]) -> Summary {
    let editable: Vec<&EntryState> = states.iter().filter(|state| !state.is_symlink).collect();
    let folders = editable.iter().filter(|state| state.is_dir).count();
    Summary {
        files: editable.len() - folders,
        folders,
        symlinks: states.len() - editable.len(),
        owner: merge(
            editable
                .iter()
                .map(|s| entry_access(s.mode, s.is_dir, Class::Owner)),
        ),
        group: merge(
            editable
                .iter()
                .map(|s| entry_access(s.mode, s.is_dir, Class::Group)),
        ),
        others: merge(
            editable
                .iter()
                .map(|s| entry_access(s.mode, s.is_dir, Class::Others)),
        ),
        exec: merge(
            editable
                .iter()
                .filter(|state| !state.is_dir)
                .map(|state| exec_of(state.mode)),
        ),
        uid: merge(editable.iter().map(|state| state.uid)),
        gid: merge(editable.iter().map(|state| state.gid)),
    }
}

/// Reads the state of `path` without following a symbolic link.
pub fn read_state(path: &Path) -> io::Result<EntryState> {
    let meta = fs::symlink_metadata(path)?;
    Ok(EntryState {
        path: path.to_path_buf(),
        is_dir: meta.is_dir(),
        is_symlink: meta.file_type().is_symlink(),
        mode: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
    })
}

/// A group the user can give an entry to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub gid: u32,
    pub name: Option<String>,
}

impl Group {
    /// The name, else the number.
    #[must_use]
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.gid.to_string())
    }
}

/// The `name:x:id:...` lines of `/etc/group` or `/etc/passwd` as `(id, name)`;
/// comments, blank and malformed lines are skipped.
#[must_use]
pub fn parse_id_file(text: &str) -> Vec<(u32, String)> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next().filter(|name| !name.is_empty())?;
            let id = fields.nth(1)?.parse().ok()?;
            Some((id, name.to_owned()))
        })
        .collect()
}

/// The effective group and supplementary groups of a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ids {
    pub primary: u32,
    pub supplementary: Vec<u32>,
}

/// Reads the `Gid:` and `Groups:` lines of `/proc/<pid>/status`.
#[must_use]
pub fn parse_status_ids(status: &str) -> Option<Ids> {
    let field = |key: &str| status.lines().find_map(|line| line.strip_prefix(key));
    // `Gid:` is real, effective, saved and file-system ids: the effective
    // one is the group new files get.
    let primary = field("Gid:")?.split_whitespace().nth(1)?.parse().ok()?;
    let supplementary = field("Groups:")
        .map(|ids| {
            ids.split_whitespace()
                .filter_map(|id| id.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    Some(Ids {
        primary,
        supplementary,
    })
}

/// The groups an entry may be given: the primary group first, then the
/// others by name (unnamed ones last, by number), each once.
#[must_use]
pub fn user_groups(ids: &Ids, names: &[(u32, String)]) -> Vec<Group> {
    let group = |gid: u32| Group {
        gid,
        name: name_of(names, gid).map(str::to_owned),
    };
    let mut others: Vec<u32> = Vec::new();
    for &gid in &ids.supplementary {
        if gid != ids.primary && !others.contains(&gid) {
            others.push(gid);
        }
    }
    let mut others: Vec<Group> = others.into_iter().map(group).collect();
    // Named groups by name, then the unnamed by number.
    others.sort_by(|a, b| {
        (a.name.is_none(), &a.name, a.gid).cmp(&(b.name.is_none(), &b.name, b.gid))
    });
    let mut groups = vec![group(ids.primary)];
    groups.extend(others);
    groups
}

/// The groups of the running process, named from `/etc/group`.
pub fn current_groups() -> io::Result<Vec<Group>> {
    let status = fs::read_to_string("/proc/self/status")?;
    let names = parse_id_file(&fs::read_to_string("/etc/group").unwrap_or_default());
    groups_from_status(&status, &names)
}

/// The groups a `/proc/<pid>/status` text lists; an error when it has no
/// group ids at all.
pub fn groups_from_status(status: &str, names: &[(u32, String)]) -> io::Result<Vec<Group>> {
    let ids = parse_status_ids(status)
        .ok_or_else(|| io::Error::other("the process groups are not in /proc/self/status"))?;
    Ok(user_groups(&ids, names))
}

/// The name of `id` in an id file's parsed lines.
#[must_use]
pub fn name_of(names: &[(u32, String)], id: u32) -> Option<&str> {
    names
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, name)| name.as_str())
}

/// What to change: the edit and, when set, the group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub edit: Edit,
    pub gid: Option<u32>,
}

/// What an entry was before it was changed, for the undo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Previous {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    /// The birth time, where the filesystem records it: ext4 hands a
    /// deleted file's inode number to the next file created, so device and
    /// inode alone cannot tell a replaced file from the edited one.
    pub born: Option<std::time::SystemTime>,
    /// The permission bits including setuid, setgid and sticky (`0o7777`).
    pub mode: u32,
    pub gid: u32,
}

/// One entry that could not be changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub path: PathBuf,
    pub message: String,
}

/// The outcome of an apply or a restore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub changed: usize,
    pub unchanged: usize,
    pub symlinks: usize,
    /// Folders not entered because they are on another filesystem.
    pub other_filesystems: usize,
    pub failures: Vec<Failure>,
    pub previous: Vec<Previous>,
    /// More changes than an undo records: no exact undo exists.
    pub undo_dropped: bool,
    pub cancelled: bool,
}

/// How many changes an undo records; past it there is no exact undo.
const MAX_UNDO_ITEMS: usize = 100_000;
/// How deep an enclosed walk goes before it reports the folder instead.
const MAX_DEPTH: usize = 512;

/// Opens `path` (relative to `dir` when given) without following a final
/// symbolic link and without needing any access to it.
fn pin(dir: Option<&File>, path: &Path) -> io::Result<File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = match dir {
        Some(dir) => openat(dir, path, flags, Mode::empty())?,
        None => openat(CWD, path, flags, Mode::empty())?,
    };
    Ok(File::from(fd))
}

/// A path that names the pinned inode itself, for the calls that take paths.
fn through(file: &File) -> PathBuf {
    use std::os::fd::AsRawFd as _;
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn chmod(file: &File, mode: u32) -> io::Result<()> {
    fs::set_permissions(through(file), fs::Permissions::from_mode(mode & 0o7777))
}

fn chgrp(file: &File, gid: u32) -> io::Result<()> {
    std::os::unix::fs::chown(through(file), None, Some(gid))
}

/// What one entry came to.
enum Outcome {
    Changed,
    Unchanged,
    Symlink,
}

struct Walk<'a> {
    change: &'a Change,
    /// How many previous states to record.
    max_undo: usize,
    cancel: &'a AtomicBool,
    on_item: &'a mut dyn FnMut(&Path, u64),
    out: Applied,
    handled: u64,
}

impl Walk<'_> {
    fn failure(&mut self, path: &Path, error: &dyn std::fmt::Display) {
        self.out.failures.push(Failure {
            path: path.to_path_buf(),
            message: error.to_string(),
        });
    }

    fn cancelled(&mut self) -> bool {
        let cancelled = self.cancel.load(Ordering::Relaxed);
        self.out.cancelled |= cancelled;
        cancelled
    }

    fn handled(&mut self, path: &Path) {
        self.handled += 1;
        (self.on_item)(path, self.handled);
    }

    /// Applies the change to one pinned entry and records what came of it.
    fn entry(&mut self, file: &File, path: &Path) {
        match self.change_one(file, path) {
            Ok(Outcome::Changed) => self.out.changed += 1,
            Ok(Outcome::Unchanged) => self.out.unchanged += 1,
            Ok(Outcome::Symlink) => self.out.symlinks += 1,
            Err(error) => self.failure(path, &error),
        }
        self.handled(path);
    }

    fn change_one(&mut self, file: &File, path: &Path) -> io::Result<Outcome> {
        let meta = file.metadata()?;
        if meta.file_type().is_symlink() {
            return Ok(Outcome::Symlink);
        }
        let before = meta.mode() & 0o7777;
        let target = apply_mode(before, meta.is_dir(), &self.change.edit) & 0o7777;
        let gid = self.change.gid.filter(|gid| *gid != meta.gid());
        if target == before && gid.is_none() {
            return Ok(Outcome::Unchanged);
        }
        if self.out.previous.len() < self.max_undo {
            self.out.previous.push(Previous {
                path: path.to_path_buf(),
                dev: meta.dev(),
                ino: meta.ino(),
                born: meta.created().ok(),
                mode: before,
                gid: meta.gid(),
            });
        } else {
            self.out.undo_dropped = true;
        }
        // The kernel clears setuid and setgid when an unprivileged chown
        // changes the group: the mode is read again and put back after it.
        let mut now = before;
        if let Some(gid) = gid {
            chgrp(file, gid)?;
            now = file.metadata()?.mode() & 0o7777;
        }
        if now != target {
            chmod(file, target)?;
        }
        Ok(Outcome::Changed)
    }

    /// Changes everything inside `dir` (a pinned folder), children before
    /// the folders that hold them, so a folder that loses its own access
    /// never blocks its contents.
    fn tree(&mut self, dir: &File, path: &Path, device: u64, depth: usize) {
        if depth > MAX_DEPTH {
            self.failure(path, &"too deeply nested");
            return;
        }
        let reader = match fs::read_dir(through(dir)) {
            Ok(reader) => reader,
            Err(error) => {
                self.failure(path, &error);
                return;
            }
        };
        let mut names = Vec::new();
        for item in reader {
            match item {
                Ok(item) => names.push(item.file_name()),
                Err(error) => self.failure(path, &error),
            }
        }
        for name in names {
            if self.cancelled() {
                return;
            }
            let child_path = path.join(&name);
            let child = match pin(Some(dir), Path::new(&name)) {
                Ok(child) => child,
                Err(error) => {
                    self.failure(&child_path, &error);
                    self.handled(&child_path);
                    continue;
                }
            };
            let meta = match child.metadata() {
                Ok(meta) => meta,
                Err(error) => {
                    self.failure(&child_path, &error);
                    self.handled(&child_path);
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                self.out.symlinks += 1;
                self.handled(&child_path);
                continue;
            }
            if meta.is_dir() {
                if meta.dev() != device {
                    self.out.other_filesystems += 1;
                    self.handled(&child_path);
                    continue;
                }
                self.tree(&child, &child_path, device, depth + 1);
                if self.out.cancelled {
                    return;
                }
            }
            self.entry(&child, &child_path);
        }
    }
}

/// Changes `roots` (never following symbolic links); with `enclosed`, every
/// folder among them also changes everything inside it, folders by the folder
/// rule and files by the file rule, staying on the folder's filesystem.
/// `on_item` is told each entry as it is handled, with the running count.
pub fn apply(
    roots: &[PathBuf],
    change: &Change,
    enclosed: bool,
    cancel: &AtomicBool,
    on_item: &mut dyn FnMut(&Path, u64),
) -> Applied {
    apply_inner(
        roots,
        change,
        enclosed,
        cancel,
        on_item,
        None,
        MAX_UNDO_ITEMS,
    )
}

/// [`apply`], with the device the enclosed walk stays on and the undo cap
/// overridable so a test can stand in for a mount or a huge tree.
fn apply_inner(
    roots: &[PathBuf],
    change: &Change,
    enclosed: bool,
    cancel: &AtomicBool,
    on_item: &mut dyn FnMut(&Path, u64),
    device: Option<u64>,
    max_undo: usize,
) -> Applied {
    let mut walk = Walk {
        change,
        max_undo,
        cancel,
        on_item,
        out: Applied::default(),
        handled: 0,
    };
    for root in roots {
        if walk.cancelled() {
            break;
        }
        let pinned = pin(None, root).and_then(|file| file.metadata().map(|meta| (file, meta)));
        let (file, meta) = match pinned {
            Ok(found) => found,
            Err(error) => {
                walk.failure(root, &error);
                walk.handled(root);
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            walk.out.symlinks += 1;
            walk.handled(root);
            continue;
        }
        if enclosed && meta.is_dir() {
            walk.tree(&file, root, device.unwrap_or(meta.dev()), 0);
            if walk.out.cancelled {
                break;
            }
        }
        walk.entry(&file, root);
    }
    walk.out
}

/// Puts back what [`apply`] recorded, newest first (so a folder is opened up
/// before the entries below it are reached), only for entries that are
/// still the same inode. `on_item` is told each entry as it is handled, with
/// the running count.
pub fn restore(
    items: &[Previous],
    cancel: &AtomicBool,
    on_item: &mut dyn FnMut(&Path, u64),
) -> Applied {
    let mut out = Applied::default();
    for (index, item) in items.iter().rev().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
            break;
        }
        match restore_one(item) {
            Ok(true) => out.changed += 1,
            Ok(false) => out.unchanged += 1,
            Err(error) => out.failures.push(Failure {
                path: item.path.clone(),
                message: error.to_string(),
            }),
        }
        on_item(&item.path, index as u64 + 1);
    }
    out
}

fn restore_one(item: &Previous) -> io::Result<bool> {
    let file = pin(None, &item.path)?;
    let meta = file.metadata()?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other("it is a symbolic link now, left alone"));
    }
    if (meta.dev(), meta.ino(), meta.created().ok()) != (item.dev, item.ino, item.born) {
        return Err(io::Error::other(
            "it changed since it was edited, left alone",
        ));
    }
    let mut done = false;
    let mut mode = meta.mode() & 0o7777;
    if meta.gid() != item.gid {
        chgrp(&file, item.gid)?;
        mode = file.metadata()?.mode() & 0o7777;
        done = true;
    }
    if mode != item.mode {
        chmod(&file, item.mode)?;
        done = true;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn edit(owner: Option<Access>, group: Option<Access>, others: Option<Access>) -> Edit {
        Edit {
            owner,
            group,
            others,
            exec: None,
        }
    }

    fn files_and_dirs(mode: u32, is_dir: bool, edit: &Edit) -> u32 {
        apply_mode(mode, is_dir, edit)
    }

    #[test]
    fn access_reads_read_and_write_per_class() {
        assert_eq!(access_of(0o640, Class::Owner), Access::ReadWrite);
        assert_eq!(access_of(0o640, Class::Group), Access::Read);
        assert_eq!(access_of(0o640, Class::Others), Access::None);
        assert_eq!(access_of(0o200, Class::Owner), Access::WriteOnly);
        // The execute bit is not part of the access.
        assert_eq!(access_of(0o111, Class::Owner), Access::None);
        assert_eq!(access_of(0o100644, Class::Others), Access::Read);
    }

    #[test]
    fn a_file_class_gets_exactly_the_chosen_read_and_write_bits() {
        let read = Some(Access::Read);
        assert_eq!(files_and_dirs(0o644, false, &edit(read, None, None)), 0o444);
        assert_eq!(
            files_and_dirs(0o600, false, &edit(None, Some(Access::ReadWrite), None)),
            0o660
        );
        assert_eq!(
            files_and_dirs(0o666, false, &edit(None, None, Some(Access::None))),
            0o660
        );
    }

    #[test]
    fn a_touched_file_class_keeps_its_execute_bit_unless_it_becomes_none() {
        // Group was r-x: rw keeps the x; none clears it (none means none).
        assert_eq!(
            files_and_dirs(0o751, false, &edit(None, Some(Access::ReadWrite), None)),
            0o771
        );
        assert_eq!(
            files_and_dirs(0o754, false, &edit(None, Some(Access::None), None)),
            0o704
        );
    }

    #[test]
    fn a_folder_maps_read_to_r_x_and_read_write_to_rwx() {
        assert_eq!(
            files_and_dirs(0o700, true, &edit(None, Some(Access::Read), None)),
            0o750
        );
        assert_eq!(
            files_and_dirs(0o700, true, &edit(None, None, Some(Access::ReadWrite))),
            0o707
        );
        assert_eq!(
            files_and_dirs(0o777, true, &edit(Some(Access::None), None, None)),
            0o077
        );
        // Write-only keeps the folder enterable, like the model says.
        assert_eq!(
            files_and_dirs(0o700, true, &edit(None, Some(Access::WriteOnly), None)),
            0o730
        );
    }

    #[test]
    fn only_the_touched_classes_change() {
        let before = 0o751;
        let after = files_and_dirs(before, false, &edit(None, None, Some(Access::ReadWrite)));
        assert_eq!(
            after, 0o757,
            "others keep their x; owner and group are as they were"
        );
        assert_eq!(files_and_dirs(before, false, &Edit::default()), before);
        assert_eq!(files_and_dirs(0o700, true, &Edit::default()), 0o700);
    }

    #[test]
    fn allow_executing_sets_x_only_for_the_classes_that_can_read() {
        let on = Edit {
            exec: Some(true),
            ..Edit::default()
        };
        assert_eq!(files_and_dirs(0o644, false, &on), 0o755);
        assert_eq!(files_and_dirs(0o600, false, &on), 0o700);
        assert_eq!(files_and_dirs(0o640, false, &on), 0o750);
        assert_eq!(files_and_dirs(0o200, false, &on), 0o200);
        let off = Edit {
            exec: Some(false),
            ..Edit::default()
        };
        assert_eq!(files_and_dirs(0o755, false, &off), 0o644);
        // A folder ignores the toggle.
        assert_eq!(files_and_dirs(0o755, true, &off), 0o755);
        assert_eq!(files_and_dirs(0o700, true, &on), 0o700);
    }

    #[test]
    fn setuid_setgid_and_sticky_survive_every_edit() {
        let all = Edit {
            owner: Some(Access::Read),
            group: Some(Access::None),
            others: Some(Access::None),
            exec: Some(false),
        };
        assert_eq!(files_and_dirs(0o4755, false, &all), 0o4400);
        assert_eq!(files_and_dirs(0o2755, false, &all), 0o2400);
        assert_eq!(files_and_dirs(0o1777, true, &all), 0o1500);
        assert_eq!(
            files_and_dirs(
                0o2755,
                false,
                &Edit {
                    exec: Some(false),
                    ..Edit::default()
                }
            ),
            0o2644
        );
        // The file type bits ride along untouched.
        assert_eq!(files_and_dirs(0o100644, false, &all), 0o100400);
    }

    #[test]
    fn exec_state_is_on_off_or_partial() {
        assert_eq!(exec_of(0o644), Exec::Off);
        assert_eq!(exec_of(0o755), Exec::On);
        assert_eq!(exec_of(0o700), Exec::On);
        assert_eq!(exec_of(0o750), Exec::On);
        assert_eq!(exec_of(0o645), Exec::Partial);
        assert_eq!(exec_of(0o100), Exec::Partial);
    }

    #[test]
    fn merge_reports_shared_and_mixed_values() {
        assert_eq!(merge([1, 1, 1]), Some(Merged::Same(1)));
        assert_eq!(merge([1, 2, 1]), Some(Merged::Mixed));
        assert_eq!(merge::<u8>([]), None);
    }

    fn state(name: &str, mode: u32, is_dir: bool, is_symlink: bool, gid: u32) -> EntryState {
        EntryState {
            path: PathBuf::from(name),
            is_dir,
            is_symlink,
            mode,
            uid: 1000,
            gid,
        }
    }

    #[test]
    fn several_entries_merge_per_class_and_exec_looks_at_files_only() {
        let states = [
            state("/a", 0o100644, false, false, 10),
            state("/b", 0o100664, false, false, 10),
            state("/d", 0o040755, true, false, 10),
            state("/l", 0o120777, false, true, 10),
        ];
        let summary = summarize(&states);
        assert_eq!(
            (summary.files, summary.folders, summary.symlinks),
            (2, 1, 1)
        );
        assert_eq!(summary.owner, Some(Merged::Same(Access::ReadWrite)));
        assert_eq!(summary.group, Some(Merged::Mixed));
        assert_eq!(summary.others, Some(Merged::Same(Access::Read)));
        assert_eq!(summary.exec, Some(Merged::Same(Exec::Off)));
        assert_eq!(summary.gid, Some(Merged::Same(10)));
        assert_eq!(summary.uid, Some(Merged::Same(1000)));

        let folders_only = summarize(&[state("/d", 0o040755, true, false, 10)]);
        assert_eq!(folders_only.exec, None, "no files, no execute toggle");
        let none = summarize(&[state("/l", 0o120777, false, true, 10)]);
        assert_eq!(none.owner, None, "a symbolic link has nothing to edit");
        let mixed_gid = summarize(&[
            state("/a", 0o100644, false, false, 10),
            state("/b", 0o100644, false, false, 11),
        ]);
        assert_eq!(mixed_gid.gid, Some(Merged::Mixed));
    }

    #[test]
    fn only_choices_that_differ_from_the_shared_values_are_applied() {
        let summary = summarize(&[
            state("/a", 0o100644, false, false, 10),
            state("/b", 0o100664, false, false, 10),
        ]);
        let draft = Edit {
            owner: Some(Access::ReadWrite), // equals the shared value
            group: Some(Access::Read),      // over mixed values
            others: Some(Access::None),     // differs from the shared Read
            exec: None,
        };
        assert_eq!(
            draft.changed_against(&summary),
            Edit {
                owner: None,
                group: Some(Access::Read),
                others: Some(Access::None),
                exec: None,
            }
        );
        assert!(Edit::default().changed_against(&summary).is_empty());
    }

    #[test]
    fn the_enclosed_rule_uses_choices_then_shared_values_and_skips_mixed() {
        let summary = summarize(&[
            state("/a", 0o040755, true, false, 10),
            state("/b", 0o040775, true, false, 10),
        ]);
        let draft = Edit {
            others: Some(Access::None),
            ..Edit::default()
        };
        assert_eq!(
            draft.effective(&summary),
            Edit {
                owner: Some(Access::ReadWrite),
                group: None, // mixed and untouched
                others: Some(Access::None),
                exec: None,
            }
        );
    }

    #[test]
    fn id_files_parse_names_and_skip_junk() {
        let text =
            "# comment\nroot:x:0:\n\nusers:x:100:alice,bob\nbroken line\nbad:x:nan:\nwheel:x:10:\n";
        assert_eq!(
            parse_id_file(text),
            vec![
                (0, "root".to_owned()),
                (100, "users".to_owned()),
                (10, "wheel".to_owned())
            ]
        );
    }

    #[test]
    fn status_ids_read_the_effective_group_and_the_supplementary_ones() {
        let status = "Name:\tx\nUid:\t1000\t1000\t1000\t1000\nGid:\t100\t101\t100\t101\nGroups:\t10 27 101 \nVmRSS:\t1 kB\n";
        assert_eq!(
            parse_status_ids(status),
            Some(Ids {
                primary: 101,
                supplementary: vec![10, 27, 101]
            })
        );
        let none = "Gid:\t5\t5\t5\t5\nGroups:\t\n";
        assert_eq!(
            parse_status_ids(none),
            Some(Ids {
                primary: 5,
                supplementary: Vec::new()
            })
        );
        assert_eq!(parse_status_ids("Name:\tx\n"), None);
    }

    #[test]
    fn the_user_groups_list_the_primary_first_then_by_name_each_once() {
        let names = vec![
            (10, "wheel".to_owned()),
            (27, "audio".to_owned()),
            (101, "users".to_owned()),
        ];
        let ids = Ids {
            primary: 101,
            supplementary: vec![10, 999, 27, 101, 5],
        };
        let groups = user_groups(&ids, &names);
        let labels: Vec<String> = groups.iter().map(Group::label).collect();
        assert_eq!(labels, ["users", "audio", "wheel", "5", "999"]);
        assert_eq!(groups.first().map(|group| group.gid), Some(101));
        assert_eq!(name_of(&names, 27), Some("audio"));
        assert_eq!(name_of(&names, 1), None);
    }

    #[test]
    fn a_folder_class_with_read_or_write_but_no_x_is_custom_not_read_only() {
        let folder = |mode: u32| summarize(&[state("/d", 0o040000 | mode, true, false, 10)]);
        assert_eq!(folder(0o750).group, Some(Merged::Same(Access::Read)));
        assert_eq!(folder(0o770).group, Some(Merged::Same(Access::ReadWrite)));
        assert_eq!(folder(0o700).group, Some(Merged::Same(Access::None)));
        for mode in [0o740, 0o720, 0o760, 0o710, 0o730] {
            assert_eq!(
                folder(mode).group,
                Some(Merged::Same(Access::Custom)),
                "{mode:o}"
            );
        }
        // A file keeps the plain read/write reading: x is not part of it.
        let file = summarize(&[state("/f", 0o100640, false, false, 10)]);
        assert_eq!(file.group, Some(Merged::Same(Access::Read)));
        assert_eq!(Access::Custom.label(), "Custom");
        assert!(!Access::CHOICES.contains(&Access::Custom));
    }

    #[test]
    fn picking_read_on_a_folder_without_x_is_a_change_that_gives_r_x() {
        let summary = summarize(&[state("/d", 0o040740, true, false, 10)]);
        let draft = Edit {
            group: Some(Access::Read),
            ..Edit::default()
        };
        let changed = draft.changed_against(&summary);
        assert_eq!(changed, draft, "Read is not what the folder has");
        assert_eq!(apply_mode(0o040740, true, &changed) & 0o777, 0o750);
        // A folder that really is r-x still has nothing to change.
        let same = summarize(&[state("/d", 0o040750, true, false, 10)]);
        assert!(draft.changed_against(&same).is_empty());
        // The enclosed rule never applies the custom value itself.
        assert_eq!(Edit::default().effective(&summary).group, None);
        assert_eq!(
            Edit::default().effective(&summary).owner,
            Some(Access::ReadWrite)
        );
    }

    #[test]
    fn the_group_list_fails_visibly_when_the_status_cannot_be_parsed() {
        let names = vec![(5, "tty".to_owned())];
        let groups = groups_from_status("Gid:\t5\t5\t5\t5\nGroups:\t\n", &names);
        assert_eq!(
            groups
                .map(|groups| groups.iter().map(Group::label).collect::<Vec<_>>())
                .ok(),
            Some(vec!["tty".to_owned()])
        );
        let broken = groups_from_status("Name:\tx\n", &names);
        assert!(broken.is_err_and(|error| error.to_string().contains("/proc/self/status")));
    }

    // --- filesystem ----------------------------------------------------------------

    /// A scratch directory under the system temp dir; unreadable folders are
    /// opened up again before it is removed on drop.
    struct Scratch {
        root: PathBuf,
    }

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch(tag: &str) -> Result<Scratch, io::Error> {
        let root = std::env::temp_dir().join(format!(
            "fileflux-perms-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        Ok(Scratch { root })
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fn open_up(path: &Path) {
                let Ok(meta) = fs::symlink_metadata(path) else {
                    return;
                };
                if meta.is_dir() {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).ok();
                    if let Ok(reader) = fs::read_dir(path) {
                        for item in reader.flatten() {
                            open_up(&item.path());
                        }
                    }
                }
            }
            open_up(&self.root);
            fs::remove_dir_all(&self.root).ok();
        }
    }

    fn mode_of(path: &Path) -> Result<u32, io::Error> {
        Ok(fs::symlink_metadata(path)?.mode() & 0o7777)
    }

    fn put(path: &Path, mode: u32) -> Result<(), io::Error> {
        fs::write(path, "x")?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    fn run(roots: &[PathBuf], change: &Change, enclosed: bool) -> Applied {
        apply(
            roots,
            change,
            enclosed,
            &AtomicBool::new(false),
            &mut |_, _| {},
        )
    }

    fn is_root() -> bool {
        fs::metadata("/proc/self").is_ok_and(|meta| meta.uid() == 0)
    }

    #[test]
    fn read_state_reports_the_entry_itself_not_a_link_target() -> TestResult {
        let dir = scratch("state")?;
        let file = dir.root.join("f");
        put(&file, 0o640)?;
        let link = dir.root.join("l");
        symlink(&file, &link)?;
        let state = read_state(&file)?;
        assert!(!state.is_dir && !state.is_symlink);
        assert_eq!(state.mode & 0o777, 0o640);
        assert!(read_state(&link)?.is_symlink);
        assert!(read_state(&dir.root)?.is_dir);
        assert!(read_state(&dir.root.join("missing")).is_err());
        Ok(())
    }

    #[test]
    fn applying_changes_the_mode_on_disk_and_keeps_special_bits() -> TestResult {
        let dir = scratch("apply")?;
        let file = dir.root.join("f");
        put(&file, 0o4755)?;
        let before = mode_of(&file)?;
        assert_eq!(before, 0o4755, "the fixture carries setuid");
        let change = Change {
            edit: edit(None, None, Some(Access::None)),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&file), &change, false);
        assert_eq!((applied.changed, applied.unchanged), (1, 0));
        assert!(applied.failures.is_empty());
        assert_eq!(mode_of(&file)?, 0o4750);
        assert_eq!(applied.previous.len(), 1);
        assert_eq!(applied.previous.first().map(|p| p.mode), Some(0o4755));
        // Nothing to change: untouched, and nothing recorded for an undo.
        let again = run(std::slice::from_ref(&file), &change, false);
        assert_eq!((again.changed, again.unchanged), (0, 1));
        assert!(again.previous.is_empty());
        assert_eq!(mode_of(&file)?, 0o4750);
        Ok(())
    }

    #[test]
    fn several_entries_change_only_their_touched_classes() -> TestResult {
        let dir = scratch("several")?;
        let a = dir.root.join("a");
        let b = dir.root.join("b");
        put(&a, 0o644)?;
        put(&b, 0o664)?;
        let change = Change {
            edit: edit(None, None, Some(Access::None)),
            gid: None,
        };
        let applied = run(&[a.clone(), b.clone()], &change, false);
        assert_eq!(applied.changed, 2);
        assert_eq!(mode_of(&a)?, 0o640);
        assert_eq!(mode_of(&b)?, 0o660, "b keeps its group write");
        Ok(())
    }

    #[test]
    fn a_symbolic_link_is_skipped_and_its_target_left_alone() -> TestResult {
        let dir = scratch("link")?;
        let target = dir.root.join("target");
        put(&target, 0o644)?;
        let link = dir.root.join("link");
        symlink(&target, &link)?;
        let change = Change {
            edit: edit(Some(Access::None), None, None),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&link), &change, false);
        assert_eq!(applied.symlinks, 1);
        assert_eq!((applied.changed, applied.unchanged), (0, 0));
        assert!(applied.failures.is_empty() && applied.previous.is_empty());
        assert_eq!(mode_of(&target)?, 0o644, "the target was not followed");
        Ok(())
    }

    #[test]
    fn enclosed_apply_uses_the_folder_rule_and_the_file_rule_and_never_follows_links() -> TestResult
    {
        let dir = scratch("tree")?;
        let outside = dir.root.join("outside");
        fs::create_dir(&outside)?;
        let outside_file = outside.join("o.txt");
        put(&outside_file, 0o644)?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700))?;
        let outside_before = mode_of(&outside)?;

        let top = dir.root.join("top");
        let sub = top.join("sub");
        fs::create_dir_all(&sub)?;
        let f1 = top.join("f1");
        let f2 = sub.join("f2");
        put(&f1, 0o755)?;
        put(&f2, 0o600)?;
        fs::set_permissions(&top, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700))?;
        symlink(&outside, top.join("dirlink"))?;
        symlink(&outside_file, sub.join("filelink"))?;

        let change = Change {
            edit: edit(None, Some(Access::Read), Some(Access::Read)),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&top), &change, true);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(applied.symlinks, 2);
        assert_eq!(mode_of(&top)?, 0o755, "folder rule: r-x");
        assert_eq!(mode_of(&sub)?, 0o755);
        assert_eq!(
            mode_of(&f1)?,
            0o755,
            "file rule keeps the x of a touched class"
        );
        assert_eq!(mode_of(&f2)?, 0o644, "file rule: r--, no x invented");
        assert_eq!(
            mode_of(&outside)?,
            outside_before,
            "the linked folder is untouched"
        );
        assert_eq!(mode_of(&outside_file)?, 0o644);
        assert_eq!(
            (applied.changed, applied.unchanged),
            (3, 1),
            "f1 already fits"
        );
        Ok(())
    }

    #[test]
    fn enclosed_apply_stays_on_the_starting_filesystem() -> TestResult {
        let dir = scratch("fs")?;
        let top = dir.root.join("top");
        let sub = top.join("sub");
        fs::create_dir_all(&sub)?;
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700))?;
        let inner = sub.join("inner");
        put(&inner, 0o600)?;
        let change = Change {
            edit: edit(None, None, Some(Access::Read)),
            gid: None,
        };
        // Pretend every folder below the start is another filesystem.
        let applied = apply_inner(
            std::slice::from_ref(&top),
            &change,
            true,
            &AtomicBool::new(false),
            &mut |_, _| {},
            Some(u64::MAX),
            MAX_UNDO_ITEMS,
        );
        assert_eq!(applied.other_filesystems, 1);
        assert_eq!(mode_of(&inner)?, 0o600, "nothing below sub was entered");
        assert_eq!(mode_of(&sub)?, 0o700, "sub itself was skipped too");
        assert_eq!(mode_of(&top)? & 0o005, 0o005, "the start folder is changed");
        Ok(())
    }

    #[test]
    fn failures_are_collected_and_the_rest_still_changes() -> TestResult {
        if is_root() {
            return Ok(());
        }
        let dir = scratch("fail")?;
        let top = dir.root.join("top");
        let locked = top.join("locked");
        fs::create_dir_all(&locked)?;
        fs::set_permissions(&top, fs::Permissions::from_mode(0o700))?;
        put(&locked.join("hidden"), 0o600)?;
        let ok = top.join("ok");
        put(&ok, 0o600)?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
        let change = Change {
            edit: edit(None, None, Some(Access::Read)),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&top), &change, true);
        assert_eq!(applied.failures.len(), 1, "{:?}", applied.failures);
        assert_eq!(
            applied.failures.first().map(|f| f.path.clone()),
            Some(locked.clone())
        );
        assert_eq!(mode_of(&ok)?, 0o604, "a sibling still changed");
        assert_eq!(mode_of(&top)?, 0o705, "and so did the folder itself");
        Ok(())
    }

    #[test]
    fn a_missing_entry_is_a_failure_not_a_panic() -> TestResult {
        let dir = scratch("missing")?;
        let change = Change {
            edit: edit(Some(Access::None), None, None),
            gid: None,
        };
        let applied = run(&[dir.root.join("gone")], &change, false);
        assert_eq!(applied.failures.len(), 1);
        assert_eq!(applied.changed, 0);
        Ok(())
    }

    #[test]
    fn cancel_stops_before_touching_anything() -> TestResult {
        let dir = scratch("cancel")?;
        let file = dir.root.join("f");
        put(&file, 0o644)?;
        let change = Change {
            edit: edit(None, None, Some(Access::None)),
            gid: None,
        };
        let applied = apply(
            std::slice::from_ref(&file),
            &change,
            false,
            &AtomicBool::new(true),
            &mut |_, _| {},
        );
        assert!(applied.cancelled);
        assert_eq!(applied.changed, 0);
        assert_eq!(mode_of(&file)?, 0o644);
        Ok(())
    }

    #[test]
    fn progress_counts_every_entry_handled() -> TestResult {
        let dir = scratch("progress")?;
        let top = dir.root.join("top");
        fs::create_dir_all(&top)?;
        put(&top.join("a"), 0o644)?;
        put(&top.join("b"), 0o644)?;
        let change = Change {
            edit: edit(None, None, Some(Access::None)),
            gid: None,
        };
        let mut seen = Vec::new();
        let applied = apply(
            std::slice::from_ref(&top),
            &change,
            true,
            &AtomicBool::new(false),
            &mut |path, count| seen.push((path.to_path_buf(), count)),
        );
        assert_eq!(applied.changed, 3);
        assert_eq!(seen.last().map(|(_, count)| *count), Some(3));
        Ok(())
    }

    #[test]
    fn the_current_groups_include_the_group_new_files_get() -> TestResult {
        let dir = scratch("groups")?;
        let file = dir.root.join("f");
        put(&file, 0o644)?;
        let gid = fs::metadata(&file)?.gid();
        let groups = current_groups()?;
        assert!(
            groups.iter().any(|group| group.gid == gid),
            "{gid} not in {groups:?}"
        );
        Ok(())
    }

    #[test]
    fn a_group_change_round_trips_and_keeps_special_bits() -> TestResult {
        let dir = scratch("group")?;
        let file = dir.root.join("f");
        put(&file, 0o2750)?;
        let original = fs::metadata(&file)?.gid();
        let groups = current_groups()?;
        // Giving the file its own group changes nothing.
        let same = Change {
            edit: Edit::default(),
            gid: Some(original),
        };
        let applied = run(std::slice::from_ref(&file), &same, false);
        assert_eq!((applied.changed, applied.unchanged), (0, 1));

        let Some(other) = groups.iter().find(|group| group.gid != original) else {
            // The user belongs to a single group: only the no-op is testable.
            return Ok(());
        };
        let change = Change {
            edit: Edit::default(),
            gid: Some(other.gid),
        };
        let applied = run(std::slice::from_ref(&file), &change, false);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(fs::metadata(&file)?.gid(), other.gid);
        assert_eq!(
            mode_of(&file)?,
            0o2750,
            "chown clears setgid; the editor puts it back"
        );
        let back = restore(&applied.previous, &AtomicBool::new(false), &mut |_, _| {});
        assert!(back.failures.is_empty(), "{:?}", back.failures);
        assert_eq!(fs::metadata(&file)?.gid(), original);
        assert_eq!(mode_of(&file)?, 0o2750);
        Ok(())
    }

    #[test]
    fn restore_puts_back_the_exact_previous_modes_of_a_whole_tree() -> TestResult {
        let dir = scratch("restore")?;
        let top = dir.root.join("top");
        let sub = top.join("sub");
        fs::create_dir_all(&sub)?;
        let f = sub.join("f");
        put(&f, 0o640)?;
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o750))?;
        fs::set_permissions(&top, fs::Permissions::from_mode(0o2775))?;
        let change = Change {
            edit: edit(Some(Access::None), Some(Access::None), Some(Access::None)),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&top), &change, true);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(
            mode_of(&top)?,
            0o2000,
            "the folder cannot even be entered now"
        );
        assert_eq!(applied.changed, 3);
        let back = restore(&applied.previous, &AtomicBool::new(false), &mut |_, _| {});
        assert!(back.failures.is_empty(), "{:?}", back.failures);
        assert_eq!(mode_of(&top)?, 0o2775);
        assert_eq!(mode_of(&sub)?, 0o750);
        assert_eq!(mode_of(&f)?, 0o640);
        Ok(())
    }

    #[test]
    fn restore_refuses_an_entry_that_is_no_longer_the_same_inode() -> TestResult {
        let dir = scratch("swap")?;
        let file = dir.root.join("f");
        put(&file, 0o644)?;
        let change = Change {
            edit: edit(None, None, Some(Access::None)),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&file), &change, false);
        fs::remove_file(&file)?;
        put(&file, 0o600)?;
        let back = restore(&applied.previous, &AtomicBool::new(false), &mut |_, _| {});
        assert_eq!(back.failures.len(), 1);
        assert!(
            back.failures
                .first()
                .is_some_and(|f| f.message.contains("changed"))
        );
        assert_eq!(mode_of(&file)?, 0o600, "the replacement is untouched");
        Ok(())
    }

    #[test]
    fn restore_never_follows_a_link_swapped_in() -> TestResult {
        let dir = scratch("swaplink")?;
        let file = dir.root.join("f");
        let target = dir.root.join("target");
        put(&file, 0o644)?;
        put(&target, 0o600)?;
        let change = Change {
            edit: edit(None, None, Some(Access::None)),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&file), &change, false);
        fs::remove_file(&file)?;
        symlink(&target, &file)?;
        let back = restore(&applied.previous, &AtomicBool::new(false), &mut |_, _| {});
        assert_eq!(back.failures.len() + back.symlinks, 1);
        assert_eq!(mode_of(&target)?, 0o600);
        Ok(())
    }

    #[test]
    fn root_dir_symlink_enclosed_is_skipped() -> TestResult {
        let dir = scratch("rootlink")?;
        let target = dir.root.join("target");
        fs::create_dir(&target)?;
        let inner = target.join("inner");
        put(&inner, 0o600)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        let link = dir.root.join("link");
        symlink(&target, &link)?;
        let change = Change {
            edit: edit(None, None, Some(Access::Read)),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&link), &change, true);
        assert_eq!(applied.symlinks, 1);
        assert_eq!(applied.changed, 0);
        assert!(applied.failures.is_empty() && applied.previous.is_empty());
        assert_eq!(mode_of(&target)?, 0o700, "the linked folder is untouched");
        assert_eq!(mode_of(&inner)?, 0o600, "and so is what is inside it");
        Ok(())
    }

    #[test]
    fn mixed_exec_toggle_and_folder_read() -> TestResult {
        let dir = scratch("mixed")?;
        let exec = dir.root.join("exec");
        let plain = dir.root.join("plain");
        let folder = dir.root.join("folder");
        put(&exec, 0o755)?;
        put(&plain, 0o640)?;
        fs::create_dir(&folder)?;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
        let change = Change {
            edit: Edit {
                group: Some(Access::Read),
                exec: Some(true),
                ..Edit::default()
            },
            gid: None,
        };
        let roots = [exec.clone(), plain.clone(), folder.clone()];
        let applied = run(&roots, &change, false);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(mode_of(&exec)?, 0o755);
        assert_eq!(mode_of(&plain)?, 0o750);
        assert_eq!(mode_of(&folder)?, 0o750);
        assert_eq!((applied.changed, applied.unchanged), (2, 1));
        let back = restore(&applied.previous, &AtomicBool::new(false), &mut |_, _| {});
        assert!(back.failures.is_empty(), "{:?}", back.failures);
        assert_eq!(mode_of(&exec)?, 0o755);
        assert_eq!(mode_of(&plain)?, 0o640);
        assert_eq!(mode_of(&folder)?, 0o700);
        Ok(())
    }

    #[test]
    fn hard_link_twice_recorded_once_and_restored() -> TestResult {
        let dir = scratch("hardlink")?;
        let first = dir.root.join("first");
        let second = dir.root.join("second");
        put(&first, 0o644)?;
        fs::hard_link(&first, &second)?;
        let change = Change {
            edit: edit(None, None, Some(Access::None)),
            gid: None,
        };
        let applied = run(&[first.clone(), second.clone()], &change, false);
        assert_eq!((applied.changed, applied.unchanged), (1, 1));
        assert_eq!(applied.previous.len(), 1, "one inode, one record");
        assert_eq!(mode_of(&second)?, 0o640);
        let back = restore(&applied.previous, &AtomicBool::new(false), &mut |_, _| {});
        assert!(back.failures.is_empty(), "{:?}", back.failures);
        assert_eq!(back.changed, 1);
        assert_eq!(mode_of(&first)?, 0o644);
        assert_eq!(mode_of(&second)?, 0o644);
        Ok(())
    }

    #[test]
    fn restore_folder_without_x_then_children() -> TestResult {
        let dir = scratch("nox")?;
        let top = dir.root.join("top");
        let sub = top.join("sub");
        fs::create_dir_all(&sub)?;
        let file = sub.join("f");
        put(&file, 0o600)?;
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o740))?;
        fs::set_permissions(&top, fs::Permissions::from_mode(0o700))?;
        let change = Change {
            edit: edit(None, Some(Access::Read), None),
            gid: None,
        };
        let applied = run(std::slice::from_ref(&top), &change, true);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(mode_of(&sub)?, 0o750, "the folder gained its x");
        assert_eq!(mode_of(&file)?, 0o640);
        let mut seen = 0;
        let back = restore(&applied.previous, &AtomicBool::new(false), &mut |_, n| {
            seen = n;
        });
        assert!(back.failures.is_empty(), "{:?}", back.failures);
        assert_eq!(seen, applied.previous.len() as u64, "progress advances");
        assert_eq!(mode_of(&sub)?, 0o740);
        assert_eq!(mode_of(&file)?, 0o600);
        assert_eq!(mode_of(&top)?, 0o700);
        Ok(())
    }

    #[test]
    fn the_undo_record_stops_at_the_cap_and_says_so() -> TestResult {
        let dir = scratch("cap")?;
        // Two fresh trees with explicit modes, so the result does not
        // depend on the process umask.
        let tree = |name: &str| -> io::Result<PathBuf> {
            let top = dir.root.join(name);
            fs::create_dir_all(&top)?;
            fs::set_permissions(&top, fs::Permissions::from_mode(0o755))?;
            for file in ["a", "b", "c"] {
                put(&top.join(file), 0o644)?;
            }
            Ok(top)
        };
        let capped_top = tree("capped")?;
        let exact_top = tree("exact")?;
        let change = Change {
            edit: edit(None, None, Some(Access::None)),
            gid: None,
        };
        let go = |top: &PathBuf, cap: usize| {
            apply_inner(
                std::slice::from_ref(top),
                &change,
                true,
                &AtomicBool::new(false),
                &mut |_, _| {},
                None,
                cap,
            )
        };
        let capped = go(&capped_top, 2);
        assert_eq!(capped.changed, 4, "everything still changed");
        assert_eq!(capped.previous.len(), 2);
        assert!(capped.undo_dropped);
        // Exactly the cap: every change is recorded and Undo is kept.
        let exact = go(&exact_top, 4);
        assert_eq!(exact.changed, 4);
        assert_eq!(exact.previous.len(), 4);
        assert!(!exact.undo_dropped);
        Ok(())
    }
}
