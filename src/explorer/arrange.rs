//! How a tab arranges its folder: the sort and the filters, as plain data.
//!
//! Nothing here touches GPUI, the filesystem or the clock (callers pass
//! `now`), and every type derives `Default` and `Clone`/`Eq`, so the whole
//! [`Arrangement`] can later be persisted to a settings file as it is. The
//! grid and the list read the one order [`Arrangement::arranged`] returns.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Datelike as _, Days, LocalResult, NaiveDate, TimeZone, Utc};
use fileflux::core::Entry;
use fileflux::core::fs::{compare_sort_prefixes, natural_sort_prefix, natural_str_cmp};
use fileflux::core::kinds::{FileCategory, extension_of};

/// The item count of each folder of a listing, when it is known: a folder
/// missing from the map was not counted yet, `None` could not be read.
pub(crate) type FolderCounts = HashMap<PathBuf, Option<usize>>;

/// What the folder is sorted by.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum SortKey {
    Name,
    Size,
    Modified,
    /// The category, then the extension, then the name.
    Type,
}

impl SortKey {
    /// Every key, in menu order.
    pub(crate) const ALL: [Self; 4] = [Self::Name, Self::Size, Self::Modified, Self::Type];

    /// The sort key of list column `column`, or `None` past the columns.
    pub(crate) fn of_column(column: usize) -> Option<Self> {
        match column {
            0 => Some(Self::Name),
            1 => Some(Self::Size),
            2 => Some(Self::Modified),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Modified => "Modified",
            Self::Type => "Type",
        }
    }
}

/// The direction of the sort.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum SortDir {
    Ascending,
    Descending,
}

impl SortDir {
    /// The arrow drawn beside the sorted header.
    pub(crate) fn arrow(self) -> &'static str {
        match self {
            Self::Ascending => "\u{2191}",
            Self::Descending => "\u{2193}",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ascending => "Ascending",
            Self::Descending => "Descending",
        }
    }

    fn flipped(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

/// The sort: a key and a direction, whether folders stay grouped before
/// files, and whether numbers inside names order by value.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct SortSpec {
    pub(crate) key: SortKey,
    pub(crate) dir: SortDir,
    /// Folders come first whatever the key or direction.
    pub(crate) folders_first: bool,
    /// `IMG_2` before `IMG_10` instead of strict character order.
    pub(crate) natural: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::Modified,
            dir: SortDir::Descending,
            folders_first: true,
            natural: true,
        }
    }
}

impl SortSpec {
    /// Whether the sort reads the folder item counts.
    fn sorts_by_size(self) -> bool {
        matches!(
            self,
            Self {
                key: SortKey::Size,
                ..
            }
        )
    }

    /// The sort after a click on `key`'s header: the active key flips its
    /// direction, any other key starts ascending. The toggles stay.
    pub(crate) fn clicked(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                dir: self.dir.flipped(),
                ..self
            }
        } else {
            Self {
                key,
                dir: SortDir::Ascending,
                ..self
            }
        }
    }
}

/// Orders two entries under `spec`, straight from the entries. The explorer
/// sorts through [`ArrangeIndex`]; this is the reference the tests prove it
/// equal to. With folders first, folders precede
/// files whatever the key or direction; within a group the key decides (a
/// missing size or time sorts lowest) and the direction reverses only that.
/// Ties fall back to the name ascending and then the raw path, so the
/// order is total and stable.
#[cfg(test)]
pub(crate) fn compare_entries(
    a: &Entry,
    b: &Entry,
    spec: SortSpec,
    counts: &FolderCounts,
) -> Ordering {
    let folders = if spec.folders_first {
        b.is_directory().cmp(&a.is_directory())
    } else {
        Ordering::Equal
    };
    let directed = |ordering: Ordering| match spec.dir {
        SortDir::Ascending => ordering,
        SortDir::Descending => ordering.reverse(),
    };
    let by_name = name_cmp(a, b, spec.natural);
    let by_key = match spec.key {
        // Name is its own key: the direction applies to the name itself.
        SortKey::Name => directed(by_name),
        SortKey::Size => directed(sort_size(a, counts).cmp(&sort_size(b, counts))),
        SortKey::Modified => directed(a.modified.cmp(&b.modified)),
        SortKey::Type => directed(
            FileCategory::of_entry(a)
                .cmp(&FileCategory::of_entry(b))
                .then_with(|| extension_of(&a.name).cmp(&extension_of(&b.name)))
                .then(by_name),
        ),
    };
    let tie = if spec.key == SortKey::Name || spec.key == SortKey::Type {
        Ordering::Equal
    } else {
        by_name
    };
    folders
        .then(by_key)
        .then(tie)
        .then_with(|| a.path.cmp(&b.path))
}

/// The size an entry sorts by: a file's bytes, or a folder's item count when
/// it is known (an uncounted folder sorts lowest).
#[cfg(test)]
fn sort_size(entry: &Entry, counts: &FolderCounts) -> Option<u64> {
    if entry.is_directory() {
        counts
            .get(&entry.path)
            .copied()
            .flatten()
            .map(|count| count as u64)
    } else {
        entry.size
    }
}

#[cfg(test)]
fn name_cmp(a: &Entry, b: &Entry, natural: bool) -> Ordering {
    if natural {
        fileflux::core::fs::natural_cmp(&a.name, &b.name)
    } else {
        a.name
            .to_string_lossy()
            .to_lowercase()
            .cmp(&b.name.to_string_lossy().to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    }
}

/// A modification-time window, in the user's local time.
#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub(crate) enum DateRange {
    Today,
    Last7Days,
    Last30Days,
    ThisYear,
    /// Before the last 30 days.
    Older,
}

impl DateRange {
    pub(crate) const ALL: [Self; 5] = [
        Self::Today,
        Self::Last7Days,
        Self::Last30Days,
        Self::ThisYear,
        Self::Older,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Last7Days => "Last 7 days",
            Self::Last30Days => "Last 30 days",
            Self::ThisYear => "This year",
            Self::Older => "Older",
        }
    }

    /// Whether `time` falls in this window, by local calendar day in
    /// `now`'s zone: `Last 7 days` is today and the six days before it, and
    /// the "since" windows include times later than now.
    pub(crate) fn matches<Tz: TimeZone>(self, time: SystemTime, now: &DateTime<Tz>) -> bool {
        let zone = now.timezone();
        let day = DateTime::<Utc>::from(time)
            .with_timezone(&zone)
            .date_naive();
        let today = now.date_naive();
        let days_ago = (today - day).num_days();
        match self {
            Self::Today => days_ago <= 0,
            Self::Last7Days => days_ago <= 6,
            Self::Last30Days => days_ago <= 29,
            Self::ThisYear => day.year() >= today.year(),
            Self::Older => days_ago >= 30,
        }
    }
}

/// A file-size window, in the SI units the rest of the app displays.
#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub(crate) enum SizeRange {
    /// Under 1 MB.
    Under1Mb,
    /// 1 MB up to (not including) 100 MB.
    From1To100Mb,
    /// 100 MB up to and including 1 GB.
    From100MbTo1Gb,
    /// More than 1 GB.
    Over1Gb,
}

impl SizeRange {
    pub(crate) const ALL: [Self; 4] = [
        Self::Under1Mb,
        Self::From1To100Mb,
        Self::From100MbTo1Gb,
        Self::Over1Gb,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Under1Mb => "< 1 MB",
            Self::From1To100Mb => "1\u{2013}100 MB",
            Self::From100MbTo1Gb => "100 MB\u{2013}1 GB",
            Self::Over1Gb => "> 1 GB",
        }
    }

    pub(crate) fn matches(self, bytes: u64) -> bool {
        const MB: u64 = 1_000_000;
        const GB: u64 = 1_000 * MB;
        match self {
            Self::Under1Mb => bytes < MB,
            Self::From1To100Mb => (MB..100 * MB).contains(&bytes),
            Self::From100MbTo1Gb => (100 * MB..=GB).contains(&bytes),
            Self::Over1Gb => bytes > GB,
        }
    }
}

/// One active filter, as a removable chip.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum FilterChip {
    Category(FileCategory),
    /// Hidden files are shown (they are hidden by default, so only the
    /// shown state is a filter change worth a chip).
    HiddenShown,
    Modified(DateRange),
    Size(SizeRange),
}

impl FilterChip {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Category(category) => category.label(),
            Self::HiddenShown => "Hidden shown",
            Self::Modified(range) => range.label(),
            Self::Size(range) => range.label(),
        }
    }
}

/// The filters of a tab. All of them combine with AND, and with the name
/// search. The default hides hidden files (dotfiles and the names a folder's
/// `.hidden` file lists) and filters nothing else.
///
/// Folders have no byte size, so a size range never applies to them. A date
/// range applies to folders only when `Folders` is the one chip selected;
/// otherwise folders pass it.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct Filters {
    /// The selected categories; none selected means every category.
    pub(crate) kinds: BTreeSet<FileCategory>,
    /// Hide dotfiles and the names a `.hidden` file lists (Ctrl+H toggles).
    pub(crate) hide_hidden: bool,
    pub(crate) modified: Option<DateRange>,
    pub(crate) size: Option<SizeRange>,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            kinds: BTreeSet::new(),
            hide_hidden: true,
            modified: None,
            size: None,
        }
    }
}

impl Filters {
    /// The filters a new tab starts with: nothing chosen, hidden files
    /// hidden. No chip stands for this state.
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether every entry passes: hidden files shown and nothing chosen.
    fn passes_everything(&self) -> bool {
        !self.hide_hidden && self.kinds.is_empty() && self.modified.is_none() && self.size.is_none()
    }

    /// The active filters, in chip order.
    pub(crate) fn chips(&self) -> Vec<FilterChip> {
        self.kinds
            .iter()
            .copied()
            .map(FilterChip::Category)
            .chain((!self.hide_hidden).then_some(FilterChip::HiddenShown))
            .chain(self.modified.map(FilterChip::Modified))
            .chain(self.size.map(FilterChip::Size))
            .collect()
    }

    /// Switches the chip's filter off.
    pub(crate) fn remove(&mut self, chip: FilterChip) {
        match chip {
            FilterChip::Category(category) => {
                self.kinds.remove(&category);
            }
            FilterChip::HiddenShown => self.hide_hidden = true,
            FilterChip::Modified(range) => {
                if self.modified == Some(range) {
                    self.modified = None;
                }
            }
            FilterChip::Size(range) => {
                if self.size == Some(range) {
                    self.size = None;
                }
            }
        }
    }

    /// Whether `entry` passes every filter at `now`, straight from the
    /// entry: the reference [`ArrangeIndex`] is proven equal to.
    #[cfg(test)]
    pub(crate) fn admits<Tz: TimeZone>(&self, entry: &Entry, now: &DateTime<Tz>) -> bool {
        if self.hide_hidden && entry.name.as_encoded_bytes().first() == Some(&b'.') {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&FileCategory::of_entry(entry)) {
            return false;
        }
        let in_date =
            |range: DateRange| entry.modified.is_some_and(|time| range.matches(time, now));
        if entry.is_directory() {
            let folders_only = self.kinds.len() == 1 && self.kinds.contains(&FileCategory::Folders);
            return self
                .modified
                .is_none_or(|range| !folders_only || in_date(range));
        }
        self.modified.is_none_or(in_date)
            && self
                .size
                .is_none_or(|range| entry.size.is_some_and(|bytes| range.matches(bytes)))
    }
}

/// One change to an [`Arrangement`], what every menu row and chip does.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum ArrangeCommand {
    SortBy(SortKey),
    SetDir(SortDir),
    ToggleFoldersFirst,
    ToggleNatural,
    ToggleCategory(FileCategory),
    /// Ctrl+H.
    ToggleHidden,
    /// Selects the range, or clears it when it is already the choice.
    ToggleDate(DateRange),
    ToggleSize(SizeRange),
    RemoveChip(FilterChip),
    ClearFilters,
}

/// A tab's sort and filters.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub(crate) struct Arrangement {
    pub(crate) sort: SortSpec,
    pub(crate) filters: Filters,
}

impl Arrangement {
    pub(crate) fn apply(&mut self, command: ArrangeCommand) {
        let filters = &mut self.filters;
        match command {
            ArrangeCommand::SortBy(key) => self.sort.key = key,
            ArrangeCommand::SetDir(dir) => self.sort.dir = dir,
            ArrangeCommand::ToggleFoldersFirst => self.sort.folders_first ^= true,
            ArrangeCommand::ToggleNatural => self.sort.natural ^= true,
            ArrangeCommand::ToggleCategory(category) => {
                if !filters.kinds.remove(&category) {
                    filters.kinds.insert(category);
                }
            }
            ArrangeCommand::ToggleHidden => filters.hide_hidden ^= true,
            ArrangeCommand::ToggleDate(range) => {
                filters.modified = (filters.modified != Some(range)).then_some(range);
            }
            ArrangeCommand::ToggleSize(range) => {
                filters.size = (filters.size != Some(range)).then_some(range);
            }
            ArrangeCommand::RemoveChip(chip) => filters.remove(chip),
            ArrangeCommand::ClearFilters => *filters = Filters::default(),
        }
    }

    /// The indices into `entries` that pass the name `query` and the
    /// filters, in sorted order, computed in one go. The explorer itself goes
    /// through [`ArrangeCache`], which keeps the index and the partial
    /// results between calls; this is the same pipeline without them.
    #[cfg(test)]
    pub(crate) fn arranged<Tz>(
        &self,
        entries: &[Entry],
        query: &str,
        counts: &FolderCounts,
        now: &DateTime<Tz>,
    ) -> Vec<usize>
    where
        Tz: TimeZone + Send + Sync + 'static,
        Tz::Offset: Send + Sync + 'static,
    {
        let inputs = ArrangeInputs::new(
            0,
            self.clone(),
            query.to_owned(),
            Arc::new(counts.clone()),
            now,
        );
        let job = ArrangeJob::new(1, inputs, Arc::new(ArrangeIndex::build(entries)), None);
        job.run().indices.as_ref().clone()
    }
}

// --- the arrangement pipeline ----------------------------------------------
//
// Arranging a folder is three steps with different lifetimes: the keys of
// every entry (once per listing), the sort (once per sort spec) and the
// filter and name scans (linear, over compact keys). A keystroke or a chip
// toggle only repeats the last step, and the name scan narrows the previous
// query's hits when the new query extends it. Nothing here reads the
// entries after `ArrangeIndex::build`, so a job can run on any thread.

/// What the filters read of one entry, and what the sorts compare.
#[derive(Debug, Clone, Copy)]
struct EntryKey {
    size: Option<u64>,
    modified: Option<SystemTime>,
    category: FileCategory,
    is_dir: bool,
    /// The name starts with a dot or a `.hidden` file lists it.
    hidden: bool,
    /// The name has an extension (possibly empty, as in `name.`).
    has_ext: bool,
}

/// Texts packed back to back, each followed by a NUL, so a scan can search
/// the whole arena at once and map a match back to its entry.
#[derive(Debug, Default)]
struct Arena {
    text: String,
    /// The offset just past each item's terminator.
    ends: Vec<usize>,
}

impl Arena {
    fn with_capacity(items: usize) -> Self {
        Self {
            text: String::with_capacity(items.saturating_mul(16)),
            ends: Vec::with_capacity(items),
        }
    }

    fn push(&mut self, item: &str) {
        self.text.push_str(item);
        self.finish_item();
    }

    /// Pushes `item` lowercased the way `str::to_lowercase` does.
    fn push_lowercase(&mut self, item: &str) {
        if item.is_ascii() {
            self.text
                .extend(item.chars().map(|c| c.to_ascii_lowercase()));
        } else {
            self.text.push_str(&item.to_lowercase());
        }
        self.finish_item();
    }

    fn finish_item(&mut self) {
        self.text.push('\0');
        self.ends.push(self.text.len());
    }

    fn get(&self, item: usize) -> &str {
        let start = item
            .checked_sub(1)
            .and_then(|previous| self.ends.get(previous))
            .copied()
            .unwrap_or(0);
        let end = self
            .ends
            .get(item)
            .map_or(start, |end| end.saturating_sub(1));
        self.text.get(start..end).unwrap_or_default()
    }
}

/// A fixed-size set of entry indices.
#[derive(Debug, Clone)]
struct Bits(Vec<u64>);

impl Bits {
    fn empty(len: usize) -> Self {
        Self(vec![0; len.div_ceil(64)])
    }

    fn full(len: usize) -> Self {
        let mut bits = Self(vec![u64::MAX; len.div_ceil(64)]);
        let used = len % 64;
        if used != 0
            && let Some(last) = bits.0.last_mut()
        {
            *last = (1u64 << used) - 1;
        }
        bits
    }

    /// How many indices are in the set.
    fn count(&self) -> usize {
        self.0.iter().map(|word| word.count_ones() as usize).sum()
    }

    fn set(&mut self, index: usize) {
        if let Some(word) = self.0.get_mut(index / 64) {
            *word |= 1 << (index % 64);
        }
    }

    fn get(&self, index: usize) -> bool {
        self.0
            .get(index / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }
}

/// The per-listing precomputation every arrangement reads: compact keys,
/// the names as plain and as lowercase text, and the few paths a tie-break
/// or a folder count needs. Built once per listing (off the UI thread, next
/// to the scan), then shared by `Arc`.
#[derive(Debug)]
pub(crate) struct ArrangeIndex {
    keys: Vec<EntryKey>,
    /// `to_string_lossy` of each name: what the natural order compares.
    display: Arena,
    /// `to_string_lossy().to_lowercase()`: what the name search and the
    /// plain name order compare.
    lower: Arena,
    /// The lowercase extension of the entries that have one.
    exts: Arena,
    /// The raw bytes of the names that are not UTF-8, by entry; every other
    /// name's raw bytes are its `display` text.
    invalid_names: HashMap<usize, Vec<u8>>,
    /// The folders and their paths, for the folder item counts.
    dirs: Vec<(usize, PathBuf)>,
    /// The entry paths, only when they are not all `parent/name` of one
    /// parent: with that layout two entries tie on the path only when their
    /// names are equal, so the path never decides anything.
    paths: Option<Vec<PathBuf>>,
}

/// Whether `path` is `prefix` joined with `name`, byte for byte.
fn is_joined(path: &[u8], prefix: &[u8], name: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    let rest = if prefix.is_empty() || prefix.ends_with(b"/") {
        rest
    } else if let Some(rest) = rest.strip_prefix(b"/") {
        rest
    } else {
        return false;
    };
    rest == name
}

impl ArrangeIndex {
    /// Indexes `entries`. Linear; blocking, so run it on the background
    /// executor, next to the scan that produced the listing.
    pub(crate) fn build(entries: &[Entry]) -> Self {
        Self::build_hiding(entries, &BTreeSet::new())
    }

    /// [`Self::build`] where the names in `hidden_names` (a folder's
    /// `.hidden` file) are hidden like dotfiles.
    pub(crate) fn build_hiding(entries: &[Entry], hidden_names: &BTreeSet<OsString>) -> Self {
        let mut keys = Vec::with_capacity(entries.len());
        let mut display = Arena::with_capacity(entries.len());
        let mut lower = Arena::with_capacity(entries.len());
        let mut exts = Arena::with_capacity(entries.len());
        let mut invalid_names = HashMap::new();
        let mut dirs = Vec::new();
        let prefix: Vec<u8> = entries
            .first()
            .and_then(|entry| entry.path.parent())
            .map(|parent| parent.as_os_str().as_encoded_bytes().to_vec())
            .unwrap_or_default();
        let mut uniform_paths = true;
        for (index, entry) in entries.iter().enumerate() {
            let name = entry.name.to_string_lossy();
            if matches!(name, Cow::Owned(_)) {
                invalid_names.insert(index, entry.name.as_encoded_bytes().to_vec());
            }
            display.push(&name);
            lower.push_lowercase(&name);
            let extension = extension_of(&entry.name);
            exts.push(extension.as_deref().unwrap_or_default());
            let is_dir = entry.is_directory();
            if is_dir {
                dirs.push((index, entry.path.clone()));
            }
            uniform_paths = uniform_paths
                && is_joined(
                    entry.path.as_os_str().as_encoded_bytes(),
                    &prefix,
                    entry.name.as_encoded_bytes(),
                );
            keys.push(EntryKey {
                size: entry.size,
                modified: entry.modified,
                category: FileCategory::of_parts(&entry.name, is_dir, extension.as_deref()),
                is_dir,
                hidden: entry.name.as_encoded_bytes().first() == Some(&b'.')
                    || hidden_names.contains(&entry.name),
                has_ext: extension.is_some(),
            });
        }
        Self {
            keys,
            display,
            lower,
            exts,
            invalid_names,
            dirs,
            paths: (!uniform_paths)
                .then(|| entries.iter().map(|entry| entry.path.clone()).collect()),
        }
    }

    /// The number of indexed entries.
    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    fn raw_name(&self, index: usize) -> &[u8] {
        self.invalid_names
            .get(&index)
            .map_or_else(|| self.display.get(index).as_bytes(), Vec::as_slice)
    }

    /// `name_cmp` of the reference, over the index.
    fn name_cmp(&self, a: usize, b: usize, natural: bool) -> Ordering {
        let by_text = if natural {
            natural_str_cmp(self.display.get(a), self.display.get(b))
        } else {
            self.lower.get(a).cmp(self.lower.get(b))
        };
        by_text.then_with(|| self.raw_name(a).cmp(self.raw_name(b)))
    }

    /// `compare_entries` of the reference, over the index. `sizes` holds the
    /// sort size of each entry and is read only by the size sort.
    fn compare(&self, a: usize, b: usize, spec: SortSpec, sizes: &[Option<u64>]) -> Ordering {
        let (Some(key_a), Some(key_b)) = (self.keys.get(a), self.keys.get(b)) else {
            return Ordering::Equal;
        };
        let SortSpec {
            key: sort_by,
            dir,
            folders_first,
            natural,
        } = spec;
        let folders = if folders_first {
            key_b.is_dir.cmp(&key_a.is_dir)
        } else {
            Ordering::Equal
        };
        let directed = |ordering: Ordering| match dir {
            SortDir::Ascending => ordering,
            SortDir::Descending => ordering.reverse(),
        };
        let by_key = match sort_by {
            SortKey::Name => directed(self.name_cmp(a, b, natural)),
            SortKey::Size => {
                let size = |index: usize| sizes.get(index).copied().flatten();
                directed(size(a).cmp(&size(b)))
            }
            SortKey::Modified => directed(key_a.modified.cmp(&key_b.modified)),
            SortKey::Type => directed(
                key_a
                    .category
                    .cmp(&key_b.category)
                    .then_with(|| {
                        let ext = |index: usize, key: &EntryKey| {
                            key.has_ext.then(|| self.exts.get(index))
                        };
                        ext(a, key_a).cmp(&ext(b, key_b))
                    })
                    .then_with(|| self.name_cmp(a, b, natural)),
            ),
        };
        let tie = if matches!(sort_by, SortKey::Name | SortKey::Type) {
            Ordering::Equal
        } else {
            self.name_cmp(a, b, natural)
        };
        folders.then(by_key).then(tie).then_with(|| {
            self.paths
                .as_ref()
                .map_or(Ordering::Equal, |paths| paths.get(a).cmp(&paths.get(b)))
        })
    }

    /// What the size sort compares: a file's bytes, a folder's item count
    /// when it is known (an uncounted folder sorts lowest).
    fn size_keys(&self, counts: &FolderCounts) -> Vec<Option<u64>> {
        let mut sizes: Vec<Option<u64>> = self.keys.iter().map(|key| key.size).collect();
        for (index, path) in &self.dirs {
            if let Some(slot) = sizes.get_mut(*index) {
                *slot = counts
                    .get(path)
                    .copied()
                    .flatten()
                    .and_then(|count| u64::try_from(count).ok());
            }
        }
        sizes
    }

    /// The distinct extensions in order, as ranks from 1 (0 is "no
    /// extension"), so the "Type" sort compares integers.
    fn extension_ranks(&self) -> HashMap<&str, u64> {
        let mut distinct: Vec<&str> = self
            .keys
            .iter()
            .enumerate()
            .filter(|(_, key)| key.has_ext)
            .map(|(index, _)| self.exts.get(index))
            .collect();
        distinct.sort_unstable();
        distinct.dedup();
        distinct.into_iter().zip(1u64..).collect()
    }

    /// One [`SortItem`] per entry, in listing order.
    fn sort_items(&self, spec: SortSpec, sizes: &[Option<u64>]) -> Vec<SortItem> {
        let SortSpec {
            key: sort_by,
            folders_first,
            natural,
            ..
        } = spec;
        let ranks = if sort_by == SortKey::Type {
            self.extension_ranks()
        } else {
            HashMap::new()
        };
        self.keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let (has, primary) = match sort_by {
                    SortKey::Name => (true, 0),
                    SortKey::Size => {
                        let size = sizes.get(index).copied().flatten();
                        (size.is_some(), size.unwrap_or(0))
                    }
                    SortKey::Modified => (
                        key.modified.is_some(),
                        key.modified.map_or(0, coarse_seconds),
                    ),
                    SortKey::Type => {
                        let rank = if key.has_ext {
                            ranks.get(self.exts.get(index)).copied().unwrap_or(0)
                        } else {
                            0
                        };
                        (true, (key.category as u64) << 32 | rank)
                    }
                };
                let text = if natural {
                    self.display.get(index)
                } else {
                    self.lower.get(index)
                };
                SortItem {
                    group: u8::from(folders_first && !key.is_dir),
                    has,
                    primary,
                    prefix: name_prefix(text, natural),
                    index,
                }
            })
            .collect()
    }

    /// Every entry in `spec` order. Ties keep listing order, as a stable
    /// sort would. The sort runs over compact items that decide most pairs
    /// without touching the names; a listing that already is in that order
    /// (the scan sorts by name, the default) costs one pass.
    fn sort(&self, spec: SortSpec, counts: &Arc<FolderCounts>) -> SortedOrder {
        let sizes = if spec.sorts_by_size() {
            self.size_keys(counts)
        } else {
            Vec::new()
        };
        let cmp = |a: &SortItem, b: &SortItem| {
            a.coarse_cmp(b, spec).unwrap_or_else(|| {
                self.compare(a.index, b.index, spec, &sizes)
                    .then(a.index.cmp(&b.index))
            })
        };
        let mut items = self.sort_items(spec, &sizes);
        if !items.is_sorted_by(|a, b| cmp(a, b) == Ordering::Less) {
            // Stable merge sort: it takes advantage of the runs a listing
            // already has (a descending sort of an ascending listing is
            // two reversed runs).
            items.sort_by(cmp);
        }
        SortedOrder {
            spec,
            counts: Arc::clone(counts),
            order: items.into_iter().map(|item| item.index).collect(),
        }
    }

    /// The entries whose lowercase name contains `needle` (lowercase, not
    /// empty), in listing order. Narrows `previous` when `needle` extends
    /// its query, since every match of the longer text matches the shorter.
    fn query_hits(&self, needle: &str, previous: Option<&QueryHits>) -> Vec<usize> {
        if let Some(previous) = previous.filter(|p| needle.contains(p.needle.as_str())) {
            return previous
                .hits
                .iter()
                .copied()
                .filter(|&index| self.lower.get(index).contains(needle))
                .collect();
        }
        if needle.contains('\0') {
            // A NUL would match across the arena's terminators.
            return (0..self.len())
                .filter(|&index| self.lower.get(index).contains(needle))
                .collect();
        }
        let mut hits = Vec::new();
        let (mut from, mut entry) = (0, 0);
        let mut chars = needle.chars();
        let single = chars.next().filter(|_| chars.next().is_none());
        while let Some(found) = self.lower.text.get(from..).and_then(|rest| {
            // A single character searches by `memchr`.
            single.map_or_else(|| rest.find(needle), |character| rest.find(character))
        }) {
            let at = from + found;
            while self.lower.ends.get(entry).is_some_and(|&end| end <= at) {
                entry += 1;
            }
            let Some(&end) = self.lower.ends.get(entry) else {
                break;
            };
            hits.push(entry);
            (from, entry) = (end, entry + 1);
        }
        hits
    }
}

/// The first sixteen bytes of a name's sort key as an integer: the natural
/// summary of [`natural_sort_prefix`], or, when the natural order is off,
/// the first bytes of the lowercase name (a name that ends is padded with
/// zeros, which order before any character).
fn name_prefix(text: &str, natural: bool) -> u128 {
    if natural {
        return natural_sort_prefix(text);
    }
    let mut bytes = [0u8; 16];
    for (slot, byte) in bytes.iter_mut().zip(text.bytes()) {
        *slot = byte;
    }
    u128::from_be_bytes(bytes)
}

/// A modification time as whole seconds in an order-preserving integer:
/// never decreasing with the time, so equal values still need the exact
/// comparison.
fn coarse_seconds(time: SystemTime) -> u64 {
    let seconds = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |s| -s),
    };
    seconds.cast_unsigned() ^ (1 << 63)
}

/// What the sort compares of one entry, packed so that sorting a million
/// of them reads contiguous memory instead of chasing each name. Every
/// field only ever *decides* a pair when it agrees with the exact
/// comparison ([`ArrangeIndex::compare`]); anything it cannot decide falls
/// back to that.
#[derive(Debug, Clone, Copy)]
struct SortItem {
    /// 0 for folders with folders first, 1 for everything else.
    group: u8,
    /// Whether the entry has a size or time (the size and time sorts).
    has: bool,
    /// The size, the time in seconds, or the category and extension rank.
    primary: u64,
    /// See [`name_prefix`].
    prefix: u128,
    index: usize,
}

impl SortItem {
    /// The order of two items when the packed fields decide it.
    fn coarse_cmp(&self, other: &Self, spec: SortSpec) -> Option<Ordering> {
        let by_group = self.group.cmp(&other.group);
        if by_group != Ordering::Equal {
            return Some(by_group);
        }
        let SortSpec {
            key: sort_by, dir, ..
        } = spec;
        let directed = |ordering: Ordering| match dir {
            SortDir::Ascending => ordering,
            SortDir::Descending => ordering.reverse(),
        };
        match sort_by {
            SortKey::Name => self.name_decided(other).map(directed),
            SortKey::Size | SortKey::Modified => {
                let ordering = (self.has, self.primary).cmp(&(other.has, other.primary));
                (ordering != Ordering::Equal).then(|| directed(ordering))
            }
            SortKey::Type => match self.primary.cmp(&other.primary) {
                Ordering::Equal => self.name_decided(other).map(directed),
                ordering => Some(directed(ordering)),
            },
        }
    }

    /// The name order when the prefixes decide it, see
    /// [`compare_sort_prefixes`].
    fn name_decided(&self, other: &Self) -> Option<Ordering> {
        compare_sort_prefixes(self.prefix, other.prefix)
    }
}

/// Every entry of a listing in one sort spec's order.
#[derive(Debug)]
pub(crate) struct SortedOrder {
    spec: SortSpec,
    /// The folder counts it was sorted under; read only by the size sort.
    counts: Arc<FolderCounts>,
    order: Vec<usize>,
}

impl SortedOrder {
    fn serves(&self, spec: SortSpec, counts: &Arc<FolderCounts>) -> bool {
        self.spec == spec && (!spec.sorts_by_size() || Arc::ptr_eq(&self.counts, counts))
    }
}

/// A modification-time window resolved for one `now`: [`DateRange::matches`]
/// as a comparison of instants. A day is a local calendar day, so the
/// boundaries are local midnights; when a midnight does not exist or is
/// ambiguous in the zone (a clock change at midnight) the window keeps
/// the exact per-entry test instead.
#[derive(Debug, Clone)]
pub(crate) struct DateWindow {
    range: DateRange,
    today: NaiveDate,
    test: DateTest,
}

#[derive(Clone)]
enum DateTest {
    Since(SystemTime),
    Before(SystemTime),
    PerEntry(Arc<dyn Fn(SystemTime) -> bool + Send + Sync>),
}

impl fmt::Debug for DateTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Since(at) => f.debug_tuple("Since").field(at).finish(),
            Self::Before(at) => f.debug_tuple("Before").field(at).finish(),
            Self::PerEntry(_) => f.write_str("PerEntry"),
        }
    }
}

impl PartialEq for DateWindow {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
            && self.today == other.today
            && match (&self.test, &other.test) {
                (DateTest::Since(a), DateTest::Since(b))
                | (DateTest::Before(a), DateTest::Before(b)) => a == b,
                (DateTest::PerEntry(_), DateTest::PerEntry(_)) => true,
                _ => false,
            }
    }
}

impl DateWindow {
    fn resolve<Tz>(range: DateRange, now: &DateTime<Tz>) -> Self
    where
        Tz: TimeZone + Send + Sync + 'static,
        Tz::Offset: Send + Sync + 'static,
    {
        let zone = now.timezone();
        let today = now.date_naive();
        let midnight = |date: Option<NaiveDate>| {
            let naive = date?.and_hms_opt(0, 0, 0)?;
            match zone.from_local_datetime(&naive) {
                LocalResult::Single(start) => Some(SystemTime::from(start)),
                LocalResult::Ambiguous(..) | LocalResult::None => None,
            }
        };
        let days_ago = |days: u64| midnight(today.checked_sub_days(Days::new(days)));
        let exact = match range {
            DateRange::Today => days_ago(0).map(DateTest::Since),
            DateRange::Last7Days => days_ago(6).map(DateTest::Since),
            DateRange::Last30Days => days_ago(29).map(DateTest::Since),
            DateRange::ThisYear => {
                midnight(NaiveDate::from_ymd_opt(today.year(), 1, 1)).map(DateTest::Since)
            }
            DateRange::Older => days_ago(29).map(DateTest::Before),
        };
        let test = exact.unwrap_or_else(|| {
            let now = now.clone();
            DateTest::PerEntry(Arc::new(move |time| range.matches(time, &now)))
        });
        Self { range, today, test }
    }

    fn matches(&self, time: SystemTime) -> bool {
        match &self.test {
            DateTest::Since(start) => time >= *start,
            DateTest::Before(start) => time < *start,
            DateTest::PerEntry(test) => test(time),
        }
    }
}

/// The filters resolved once for a scan over the keys.
struct Admission<'a> {
    filters: &'a Filters,
    window: Option<&'a DateWindow>,
    /// The bit of each selected category; zero selects every category.
    kinds: u8,
    /// `Folders` is the one chip selected: a date range then applies to
    /// folders too.
    folders_only: bool,
}

impl<'a> Admission<'a> {
    fn new(filters: &'a Filters, window: Option<&'a DateWindow>) -> Self {
        Self {
            filters,
            window,
            kinds: filters
                .kinds
                .iter()
                .fold(0, |mask, &category| mask | Self::bit(category)),
            folders_only: filters.kinds.len() == 1
                && filters.kinds.contains(&FileCategory::Folders),
        }
    }

    fn bit(category: FileCategory) -> u8 {
        1 << (category as u8)
    }

    /// [`Filters::admits`] over an entry's key.
    fn admits(&self, key: &EntryKey) -> bool {
        if self.filters.hide_hidden && key.hidden {
            return false;
        }
        if self.kinds != 0 && self.kinds & Self::bit(key.category) == 0 {
            return false;
        }
        let in_date = || {
            key.modified
                .is_some_and(|time| self.window.is_some_and(|window| window.matches(time)))
        };
        let undated = self.filters.modified.is_none();
        if key.is_dir {
            return undated || !self.folders_only || in_date();
        }
        (undated || in_date())
            && self
                .filters
                .size
                .is_none_or(|range| key.size.is_some_and(|bytes| range.matches(bytes)))
    }
}

/// The entries that pass a set of filters.
#[derive(Debug)]
pub(crate) struct FilterPass {
    filters: Filters,
    window: Option<DateWindow>,
    bits: Bits,
}

impl FilterPass {
    fn compute(index: &ArrangeIndex, filters: &Filters, window: Option<&DateWindow>) -> Self {
        let bits = if filters.passes_everything() {
            Bits::full(index.len())
        } else {
            let admission = Admission::new(filters, window);
            let mut bits = Bits::empty(index.len());
            for (position, key) in index.keys.iter().enumerate() {
                if admission.admits(key) {
                    bits.set(position);
                }
            }
            bits
        };
        Self {
            filters: filters.clone(),
            window: window.cloned(),
            bits,
        }
    }

    fn serves(&self, filters: &Filters, window: Option<&DateWindow>) -> bool {
        self.filters == *filters && self.window.as_ref() == window
    }
}

/// The entries a name query matched, in listing order.
#[derive(Debug)]
pub(crate) struct QueryHits {
    /// The query, lowercase.
    needle: String,
    hits: Vec<usize>,
}

/// Everything an arrangement depends on. Two results with equal inputs are
/// the same list, so a result is current exactly when its inputs equal the
/// state's now.
#[derive(Debug, Clone)]
pub(crate) struct ArrangeInputs {
    /// Which listing: bumped by the state on every commit.
    epoch: u64,
    arrangement: Arrangement,
    query: String,
    /// Compared by identity: the folder counts are replaced, never edited.
    counts: Arc<FolderCounts>,
    /// Only with a date filter on, so the clock does not invalidate
    /// anything else.
    window: Option<DateWindow>,
}

impl ArrangeInputs {
    pub(crate) fn new<Tz>(
        epoch: u64,
        arrangement: Arrangement,
        query: String,
        counts: Arc<FolderCounts>,
        now: &DateTime<Tz>,
    ) -> Self
    where
        Tz: TimeZone + Send + Sync + 'static,
        Tz::Offset: Send + Sync + 'static,
    {
        let window = arrangement
            .filters
            .modified
            .map(|range| DateWindow::resolve(range, now));
        Self {
            epoch,
            arrangement,
            query,
            counts,
            window,
        }
    }
}

impl PartialEq for ArrangeInputs {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch
            && self.arrangement == other.arrangement
            && self.query == other.query
            && Arc::ptr_eq(&self.counts, &other.counts)
            && self.window == other.window
    }
}

/// The partial results of earlier arrangements of one listing that a new
/// one may reuse, each when its own inputs still match.
#[derive(Debug, Clone, Default)]
struct Warm {
    epoch: u64,
    sorted: Option<Arc<SortedOrder>>,
    filter: Option<Arc<FilterPass>>,
    hits: Option<Arc<QueryHits>>,
}

/// One arrangement to compute, pure: it owns its inputs and the index by
/// `Arc`, so it can run on the background executor. It carries the
/// generation it was started under; hand the result to
/// [`ArrangeCache::commit`], which drops it when it is stale.
#[derive(Debug)]
pub(crate) struct ArrangeJob {
    generation: u64,
    inputs: ArrangeInputs,
    index: Arc<ArrangeIndex>,
    warm: Warm,
}

/// A finished [`ArrangeJob`].
#[derive(Debug, Clone)]
pub(crate) struct ArrangeOutput {
    generation: u64,
    inputs: ArrangeInputs,
    /// The entry indices that pass, in displayed order.
    indices: Arc<Vec<usize>>,
    warm: Warm,
}

impl ArrangeOutput {
    /// The generation the job was started under.
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

impl ArrangeJob {
    fn new(
        generation: u64,
        inputs: ArrangeInputs,
        index: Arc<ArrangeIndex>,
        warm: Option<&Warm>,
    ) -> Self {
        let warm = warm
            .filter(|warm| warm.epoch == inputs.epoch)
            .cloned()
            .unwrap_or_default();
        Self {
            generation,
            inputs,
            index,
            warm,
        }
    }

    /// The generation this job was started under.
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Computes the arrangement. Blocking: linear in the listing, plus one
    /// sort when the sort spec is new.
    pub(crate) fn run(self) -> ArrangeOutput {
        let Self {
            generation,
            inputs,
            index,
            warm,
        } = self;
        let spec = inputs.arrangement.sort;
        let sorted = warm
            .sorted
            .filter(|sorted| sorted.serves(spec, &inputs.counts))
            .unwrap_or_else(|| Arc::new(index.sort(spec, &inputs.counts)));
        let filters = &inputs.arrangement.filters;
        let filter = warm
            .filter
            .filter(|pass| pass.serves(filters, inputs.window.as_ref()))
            .unwrap_or_else(|| {
                Arc::new(FilterPass::compute(&index, filters, inputs.window.as_ref()))
            });
        let needle = inputs.query.to_lowercase();
        let hits = (!needle.is_empty()).then(|| {
            match warm.hits.clone().filter(|hits| hits.needle == needle) {
                Some(hits) => hits,
                None => {
                    let found = index.query_hits(&needle, warm.hits.as_deref());
                    Arc::new(QueryHits {
                        needle,
                        hits: found,
                    })
                }
            }
        });
        let narrowed = hits.as_ref().map(|hits| {
            let mut pass = Bits::empty(index.len());
            for &hit in &hits.hits {
                if filter.bits.get(hit) {
                    pass.set(hit);
                }
            }
            pass
        });
        let passes = narrowed.as_ref().unwrap_or(&filter.bits);
        let mut indices = Vec::with_capacity(passes.count());
        indices.extend(
            sorted
                .order
                .iter()
                .copied()
                .filter(|&entry| passes.get(entry)),
        );
        ArrangeOutput {
            generation,
            indices: Arc::new(indices),
            warm: Warm {
                epoch: inputs.epoch,
                sorted: Some(sorted),
                filter: Some(filter),
                hits,
            },
            inputs,
        }
    }
}

/// A tab's arrangement results and the generation guard around them: what
/// is shown, which job is in flight, and what a new job may reuse.
///
/// The UI drives it in two steps, like a scan: [`Self::begin`] turns the
/// current inputs into an [`ArrangeJob`] to run off the UI thread, and
/// [`Self::commit`] settles its [`ArrangeOutput`]. A stale output is never
/// shown: it must be of the newest job, for the listing and inputs the state
/// has now. Without a job in flight, [`Self::resolve`] computes in place, so
/// callers that never begin a job (tests, small folders) still get the
/// answer immediately.
#[derive(Debug, Clone, Default)]
pub(crate) struct ArrangeCache {
    index: Option<(u64, Arc<ArrangeIndex>)>,
    shown: Option<ArrangeOutput>,
    warm: Warm,
    /// Bumped by every job that begins; outputs compare against it.
    generation: u64,
    pending: Option<(u64, ArrangeInputs)>,
    /// The order of the entries a listing had before more were appended
    /// ([`Self::grow`]): shown while the job for the longer listing runs.
    grown_from: Option<Arc<Vec<usize>>>,
}

impl ArrangeCache {
    /// A new listing replaced the old one: everything cached is dropped,
    /// and a job still running for the old one will be refused. `index` is
    /// the listing's index when the scan built it already.
    pub(crate) fn reset(&mut self, epoch: u64, index: Option<Arc<ArrangeIndex>>) {
        self.index = index.map(|index| (epoch, index));
        self.shown = None;
        self.warm = Warm::default();
        self.pending = None;
        self.grown_from = None;
    }

    /// Entries were appended to the listing (streamed search results): the
    /// index and the partial results are dropped, but the order of the
    /// entries that were there stays on screen — their indices still name
    /// the same entries — until the job for the longer listing lands, so a
    /// growing list does not flash empty.
    pub(crate) fn grow(&mut self) {
        if let Some(shown) = self.shown.take() {
            self.grown_from = Some(shown.indices);
        }
        self.index = None;
        self.warm = Warm::default();
        self.pending = None;
    }

    /// A job is running; [`Self::resolve`] keeps showing the last result
    /// meanwhile.
    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Gives up the job in flight (its task was dropped), so the next
    /// [`Self::begin`] starts a new one.
    pub(crate) fn cancel(&mut self) {
        self.pending = None;
    }

    fn index_for(&mut self, epoch: u64, entries: &[Entry]) -> Arc<ArrangeIndex> {
        match &self.index {
            Some((built_for, index)) if *built_for == epoch && index.len() == entries.len() => {
                Arc::clone(index)
            }
            _ => {
                let index = Arc::new(ArrangeIndex::build(entries));
                self.index = Some((epoch, Arc::clone(&index)));
                index
            }
        }
    }

    /// The job that makes `inputs` current, or `None` when the shown result
    /// already is, or a job for exactly these inputs is running. Builds the
    /// index here when the scan did not (that is the one linear step left
    /// on the calling thread).
    pub(crate) fn begin(&mut self, inputs: ArrangeInputs, entries: &[Entry]) -> Option<ArrangeJob> {
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.inputs == inputs)
        {
            return None;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, pending)| *pending == inputs)
        {
            return None;
        }
        let index = self.index_for(inputs.epoch, entries);
        self.generation += 1;
        self.pending = Some((self.generation, inputs.clone()));
        Some(ArrangeJob::new(
            self.generation,
            inputs,
            index,
            Some(&self.warm),
        ))
    }

    /// Settles a job's output against the inputs the state has `current`.
    /// `false` (nothing shown) when it is stale: a newer job began since, or
    /// the listing, query or arrangement moved on while it ran. The partial
    /// results it computed for the current listing are kept for the next
    /// job either way.
    pub(crate) fn commit(&mut self, output: ArrangeOutput, current: &ArrangeInputs) -> bool {
        if output.inputs.epoch == current.epoch {
            self.warm = output.warm.clone();
        }
        if output.generation != self.generation {
            return false;
        }
        self.pending = None;
        if output.inputs != *current {
            return false;
        }
        self.shown = Some(output);
        self.grown_from = None;
        true
    }

    /// The entry indices for `inputs`, in displayed order: the shown result
    /// when it is current; the last shown result of this listing while a
    /// job runs (nothing yet for a fresh listing); otherwise computed now.
    pub(crate) fn resolve(&mut self, inputs: &ArrangeInputs, entries: &[Entry]) -> Arc<Vec<usize>> {
        if let Some(shown) = self.shown.as_ref().filter(|shown| shown.inputs == *inputs) {
            return Arc::clone(&shown.indices);
        }
        if self.pending.is_some() {
            return self
                .shown
                .as_ref()
                .filter(|shown| shown.inputs.epoch == inputs.epoch)
                .map(|shown| Arc::clone(&shown.indices))
                .or_else(|| self.grown_from.clone())
                .unwrap_or_default();
        }
        let index = self.index_for(inputs.epoch, entries);
        self.generation += 1;
        let job = ArrangeJob::new(self.generation, inputs.clone(), index, Some(&self.warm));
        let output = job.run();
        self.warm = output.warm.clone();
        let indices = Arc::clone(&output.indices);
        self.shown = Some(output);
        indices
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{by_name, shown};
    use super::*;
    use chrono::FixedOffset;
    use fileflux::core::EntryKind;
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::Duration;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn zone() -> Result<FixedOffset, Box<dyn std::error::Error>> {
        FixedOffset::east_opt(2 * 3_600).ok_or_else(|| "offset out of range".into())
    }

    /// 2026-11-18 (Wednesday) 12:00 in a UTC+2 zone.
    fn now() -> Result<DateTime<FixedOffset>, Box<dyn std::error::Error>> {
        Ok(zone()?
            .with_ymd_and_hms(2026, 11, 18, 12, 0, 0)
            .single()
            .ok_or("ambiguous test instant")?)
    }

    fn local(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
    ) -> Result<SystemTime, Box<dyn std::error::Error>> {
        Ok(zone()?
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .ok_or("ambiguous test instant")?
            .into())
    }

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: OsString::from(name),
            path: Path::new("/fx").join(name),
            kind: EntryKind::File,
            is_symlink: false,
            size: Some(size),
            modified: None,
            origin: None,
        }
    }

    fn dir(name: &str) -> Entry {
        Entry {
            kind: EntryKind::Directory,
            size: None,
            ..file(name, 0)
        }
    }

    fn at(entry: Entry, time: SystemTime) -> Entry {
        Entry {
            modified: Some(time),
            ..entry
        }
    }

    fn names(entries: &[Entry], order: &[usize]) -> Vec<String> {
        order
            .iter()
            .filter_map(|&index| entries.get(index))
            .map(|entry| entry.name.to_string_lossy().into_owned())
            .collect()
    }

    fn arranged_with(
        entries: &[Entry],
        arrangement: &Arrangement,
        query: &str,
        counts: &FolderCounts,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let order = arrangement.arranged(entries, query, counts, &now()?);
        Ok(names(entries, &order))
    }

    fn arranged(
        entries: &[Entry],
        arrangement: &Arrangement,
        query: &str,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        arranged_with(entries, arrangement, query, &FolderCounts::new())
    }

    fn sort(key: SortKey, dir: SortDir, folders_first: bool) -> Arrangement {
        Arrangement {
            sort: SortSpec {
                key,
                dir,
                folders_first,
                natural: true,
            },
            ..Arrangement::default()
        }
    }

    /// Two folders, files of different sizes, times and types.
    fn fixture() -> Result<Vec<Entry>, Box<dyn std::error::Error>> {
        Ok(vec![
            at(file("zed.txt", 10), local(2026, 1, 1, 9)?),
            at(dir("b-dir"), local(2026, 11, 1, 9)?),
            at(file("apple.png", 500), local(2026, 11, 17, 9)?),
            at(dir("a-dir"), local(2025, 5, 5, 9)?),
            at(file("mid.mp3", 50), local(2026, 6, 1, 9)?),
            file("nodata", 0),
        ])
    }

    #[test]
    fn name_sort_goes_both_ways_with_and_without_folders_first() -> TestResult {
        let list = fixture()?;
        let order = |dir, first| arranged(&list, &sort(SortKey::Name, dir, first), "");
        assert_eq!(
            order(SortDir::Ascending, true)?,
            [
                "a-dir",
                "b-dir",
                "apple.png",
                "mid.mp3",
                "nodata",
                "zed.txt"
            ]
        );
        assert_eq!(
            order(SortDir::Descending, true)?,
            [
                "b-dir",
                "a-dir",
                "zed.txt",
                "nodata",
                "mid.mp3",
                "apple.png"
            ]
        );
        assert_eq!(
            order(SortDir::Ascending, false)?,
            [
                "a-dir",
                "apple.png",
                "b-dir",
                "mid.mp3",
                "nodata",
                "zed.txt"
            ]
        );
        assert_eq!(
            order(SortDir::Descending, false)?,
            [
                "zed.txt",
                "nodata",
                "mid.mp3",
                "b-dir",
                "apple.png",
                "a-dir"
            ]
        );
        Ok(())
    }

    #[test]
    fn size_sort_uses_file_bytes_and_folder_item_counts() -> TestResult {
        let list = fixture()?;
        let mut counts = FolderCounts::new();
        counts.insert(Path::new("/fx/b-dir").to_path_buf(), Some(3));
        let order =
            |dir, first| arranged_with(&list, &sort(SortKey::Size, dir, first), "", &counts);
        // a-dir is uncounted (lowest); b-dir counts 3.
        assert_eq!(
            order(SortDir::Ascending, true)?,
            [
                "a-dir",
                "b-dir",
                "nodata",
                "zed.txt",
                "mid.mp3",
                "apple.png"
            ]
        );
        assert_eq!(
            order(SortDir::Descending, true)?,
            [
                "b-dir",
                "a-dir",
                "apple.png",
                "mid.mp3",
                "zed.txt",
                "nodata"
            ]
        );
        // Mixed: the folder's 3 items sit between the 0-byte and 10-byte files.
        assert_eq!(
            order(SortDir::Ascending, false)?,
            [
                "a-dir",
                "nodata",
                "b-dir",
                "zed.txt",
                "mid.mp3",
                "apple.png"
            ]
        );
        Ok(())
    }

    #[test]
    fn modified_sort_puts_unknown_times_lowest() -> TestResult {
        let list = fixture()?;
        let order = |dir, first| arranged(&list, &sort(SortKey::Modified, dir, first), "");
        assert_eq!(
            order(SortDir::Ascending, true)?,
            [
                "a-dir",
                "b-dir",
                "nodata",
                "zed.txt",
                "mid.mp3",
                "apple.png"
            ]
        );
        assert_eq!(
            order(SortDir::Descending, false)?,
            [
                "apple.png",
                "b-dir",
                "mid.mp3",
                "zed.txt",
                "a-dir",
                "nodata"
            ]
        );
        Ok(())
    }

    #[test]
    fn type_sort_orders_category_then_extension_then_name() -> TestResult {
        let list = vec![
            file("b.txt", 1),
            file("a.rs", 1),
            file("z.png", 1),
            file("a.png", 1),
            file("c.jpg", 1),
            file("movie.mp4", 1),
            file("song.mp3", 1),
            file("README", 1),
            dir("d"),
        ];
        assert_eq!(
            arranged(&list, &sort(SortKey::Type, SortDir::Ascending, true), "")?,
            [
                "d",
                "c.jpg",
                "a.png",
                "z.png",
                "movie.mp4",
                "song.mp3",
                "b.txt",
                "a.rs",
                "README"
            ]
        );
        assert_eq!(
            arranged(&list, &sort(SortKey::Type, SortDir::Descending, false), "")?,
            [
                "README",
                "a.rs",
                "b.txt",
                "song.mp3",
                "movie.mp4",
                "z.png",
                "a.png",
                "c.jpg",
                "d"
            ]
        );
        Ok(())
    }

    #[test]
    fn natural_order_is_a_toggle() -> TestResult {
        let list = vec![file("IMG_10", 1), file("IMG_2", 1), file("IMG_1", 1)];
        let mut arrangement = Arrangement::default();
        assert_eq!(
            arranged(&list, &arrangement, "")?,
            ["IMG_1", "IMG_2", "IMG_10"]
        );
        arrangement.apply(ArrangeCommand::ToggleNatural);
        assert!(!arrangement.sort.natural);
        assert_eq!(
            arranged(&list, &arrangement, "")?,
            ["IMG_1", "IMG_10", "IMG_2"]
        );
        Ok(())
    }

    #[test]
    fn header_clicks_set_or_flip_and_keep_the_toggles() {
        let spec = SortSpec {
            key: SortKey::Name,
            dir: SortDir::Ascending,
            folders_first: false,
            natural: false,
        };
        let flipped = spec.clicked(SortKey::Name);
        assert_eq!(
            (flipped.key, flipped.dir),
            (SortKey::Name, SortDir::Descending)
        );
        let size = spec.clicked(SortKey::Size);
        assert_eq!((size.key, size.dir), (SortKey::Size, SortDir::Ascending));
        assert!(!size.folders_first && !size.natural);
        assert_eq!(SortKey::of_column(3), None);
        assert_eq!(SortKey::of_column(2), Some(SortKey::Modified));
    }

    #[test]
    fn the_default_arrangement_is_newest_first_with_hidden_files_hidden() -> TestResult {
        let arrangement = Arrangement::default();
        assert_eq!(
            (arrangement.sort.key, arrangement.sort.dir),
            (SortKey::Modified, SortDir::Descending)
        );
        assert!(arrangement.filters.is_default());
        assert!(arrangement.sort.folders_first && arrangement.sort.natural);
        assert!(arrangement.filters.hide_hidden, "hidden files are hidden");
        assert!(
            arrangement.filters.chips().is_empty(),
            "no chip in the default state"
        );
        let list = mixed()?;
        assert_eq!(
            arranged(&list, &arrangement, "")?,
            ["pics", "a.png", "b.PNG", "c.mp4", "d.zip", "e.rs"],
            "folders first, then newest first, no .config or .hidden.txt"
        );
        // Showing them adds the one chip, and removing it hides them again.
        let mut filters = shown();
        assert_eq!(filters.chips(), [FilterChip::HiddenShown]);
        assert_eq!(FilterChip::HiddenShown.label(), "Hidden shown");
        filters.remove(FilterChip::HiddenShown);
        assert!(filters.is_default() && filters.chips().is_empty());
        let everything = Arrangement {
            filters: shown(),
            ..Arrangement::default()
        };
        assert_eq!(arranged(&list, &everything, "")?.len(), list.len());
        Ok(())
    }

    fn mixed() -> Result<Vec<Entry>, Box<dyn std::error::Error>> {
        Ok(vec![
            at(dir("pics"), local(2026, 11, 18, 8)?),
            at(dir(".config"), local(2020, 1, 1, 8)?),
            at(file("a.png", 500_000), local(2026, 11, 18, 9)?),
            at(file("b.PNG", 5_000_000), local(2026, 11, 12, 9)?),
            at(file("c.mp4", 150_000_000), local(2026, 10, 25, 9)?),
            at(file("d.zip", 2_000_000_000), local(2026, 3, 1, 9)?),
            at(file(".hidden.txt", 10), local(2025, 3, 1, 9)?),
            at(file("e.rs", 1_000_000_000), local(2025, 12, 31, 9)?),
        ])
    }

    fn with(filters: Filters) -> Arrangement {
        Arrangement {
            filters,
            ..by_name()
        }
    }

    #[test]
    fn each_filter_alone() -> TestResult {
        let list = mixed()?;
        let kinds = |set: &[FileCategory]| {
            with(Filters {
                kinds: set.iter().copied().collect(),
                ..shown()
            })
        };
        assert_eq!(
            arranged(&list, &kinds(&[FileCategory::Images]), "")?,
            ["a.png", "b.PNG"]
        );
        assert_eq!(
            arranged(
                &list,
                &kinds(&[FileCategory::Folders, FileCategory::Code]),
                ""
            )?,
            [".config", "pics", "e.rs"]
        );
        assert_eq!(
            arranged(
                &list,
                &with(Filters {
                    hide_hidden: true,
                    ..shown()
                }),
                ""
            )?,
            ["pics", "a.png", "b.PNG", "c.mp4", "d.zip", "e.rs"]
        );
        let date = |range| {
            with(Filters {
                modified: Some(range),
                ..shown()
            })
        };
        assert_eq!(
            arranged(&list, &date(DateRange::Today), "")?,
            [".config", "pics", "a.png"]
        );
        assert_eq!(
            arranged(&list, &date(DateRange::Last7Days), "")?,
            [".config", "pics", "a.png", "b.PNG"]
        );
        assert_eq!(
            arranged(&list, &date(DateRange::Last30Days), "")?,
            [".config", "pics", "a.png", "b.PNG", "c.mp4"]
        );
        assert_eq!(
            arranged(&list, &date(DateRange::ThisYear), "")?,
            [".config", "pics", "a.png", "b.PNG", "c.mp4", "d.zip"]
        );
        assert_eq!(
            arranged(&list, &date(DateRange::Older), "")?,
            [".config", "pics", ".hidden.txt", "d.zip", "e.rs"]
        );
        let size = |range| {
            with(Filters {
                size: Some(range),
                ..shown()
            })
        };
        assert_eq!(
            arranged(&list, &size(SizeRange::Under1Mb), "")?,
            [".config", "pics", ".hidden.txt", "a.png"]
        );
        assert_eq!(
            arranged(&list, &size(SizeRange::From1To100Mb), "")?,
            [".config", "pics", "b.PNG"]
        );
        assert_eq!(
            arranged(&list, &size(SizeRange::From100MbTo1Gb), "")?,
            [".config", "pics", "c.mp4", "e.rs"]
        );
        assert_eq!(
            arranged(&list, &size(SizeRange::Over1Gb), "")?,
            [".config", "pics", "d.zip"]
        );
        Ok(())
    }

    #[test]
    fn date_filters_apply_to_folders_only_when_folders_is_the_one_chip() -> TestResult {
        let list = mixed()?;
        let only_folders = with(Filters {
            kinds: BTreeSet::from([FileCategory::Folders]),
            modified: Some(DateRange::Today),
            ..shown()
        });
        assert_eq!(arranged(&list, &only_folders, "")?, ["pics"]);
        // A byte-size range never applies to folders, even then.
        let only_folders_sized = with(Filters {
            kinds: BTreeSet::from([FileCategory::Folders]),
            size: Some(SizeRange::Over1Gb),
            ..shown()
        });
        assert_eq!(
            arranged(&list, &only_folders_sized, "")?,
            [".config", "pics"]
        );
        Ok(())
    }

    #[test]
    fn filters_combine_with_each_other_and_with_the_name_search() -> TestResult {
        let list = mixed()?;
        let combined = with(Filters {
            kinds: BTreeSet::from([FileCategory::Images, FileCategory::Videos]),
            modified: Some(DateRange::Last30Days),
            size: Some(SizeRange::From1To100Mb),
            hide_hidden: true,
        });
        assert_eq!(arranged(&list, &combined, "")?, ["b.PNG"]);
        let loose = with(Filters {
            kinds: BTreeSet::from([FileCategory::Images, FileCategory::Videos]),
            ..shown()
        });
        assert_eq!(arranged(&list, &loose, "PNG")?, ["a.png", "b.PNG"]);
        assert_eq!(arranged(&list, &loose, "b.")?, ["b.PNG"]);
        assert_eq!(arranged(&list, &loose, "zzz")?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn a_file_without_metadata_fails_an_active_date_or_size_filter() -> TestResult {
        let unknown = vec![Entry {
            size: None,
            ..file("nodata", 0)
        }];
        for filters in [
            Filters {
                modified: Some(DateRange::Older),
                ..shown()
            },
            Filters {
                size: Some(SizeRange::Under1Mb),
                ..shown()
            },
        ] {
            assert!(arranged(&unknown, &with(filters), "")?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn date_windows_follow_local_calendar_days() -> TestResult {
        let now = now()?;
        // 23:30 UTC on the 17th is already the 18th at UTC+2.
        let late: SystemTime = Utc
            .with_ymd_and_hms(2026, 11, 17, 23, 30, 0)
            .single()
            .ok_or("ambiguous")?
            .into();
        assert!(DateRange::Today.matches(late, &now));
        assert!(!DateRange::Today.matches(local(2026, 11, 17, 23)?, &now));
        assert!(DateRange::Last7Days.matches(local(2026, 11, 12, 0)?, &now));
        assert!(!DateRange::Last7Days.matches(local(2026, 11, 11, 23)?, &now));
        assert!(DateRange::Last30Days.matches(local(2026, 10, 20, 0)?, &now));
        assert!(!DateRange::Last30Days.matches(local(2026, 10, 19, 23)?, &now));
        assert!(DateRange::ThisYear.matches(local(2026, 1, 1, 0)?, &now));
        assert!(!DateRange::ThisYear.matches(local(2025, 12, 31, 23)?, &now));
        assert!(DateRange::Older.matches(local(2026, 10, 19, 23)?, &now));
        assert!(!DateRange::Older.matches(local(2026, 10, 20, 0)?, &now));
        // A time in the future still counts as recent, never as older.
        let future: SystemTime = (now.with_timezone(&Utc) + Duration::from_secs(86_400 * 3)).into();
        assert!(DateRange::Today.matches(future, &now));
        assert!(!DateRange::Older.matches(future, &now));
        Ok(())
    }

    #[test]
    fn size_windows_use_si_units_and_meet_without_gaps() {
        for (bytes, expected) in [
            (0, SizeRange::Under1Mb),
            (999_999, SizeRange::Under1Mb),
            (1_000_000, SizeRange::From1To100Mb),
            (99_999_999, SizeRange::From1To100Mb),
            (100_000_000, SizeRange::From100MbTo1Gb),
            (1_000_000_000, SizeRange::From100MbTo1Gb),
            (1_000_000_001, SizeRange::Over1Gb),
        ] {
            let hits: Vec<SizeRange> = SizeRange::ALL
                .into_iter()
                .filter(|range| range.matches(bytes))
                .collect();
            assert_eq!(hits, [expected], "{bytes}");
        }
    }

    #[test]
    fn chips_name_the_active_filters_and_remove_one_by_one() {
        let mut filters = Filters {
            kinds: BTreeSet::from([FileCategory::Videos, FileCategory::Images]),
            hide_hidden: false,
            modified: Some(DateRange::Last7Days),
            size: Some(SizeRange::Over1Gb),
        };
        let labels: Vec<&str> = filters.chips().into_iter().map(FilterChip::label).collect();
        assert_eq!(
            labels,
            ["Images", "Videos", "Hidden shown", "Last 7 days", "> 1 GB"]
        );
        filters.remove(FilterChip::Category(FileCategory::Images));
        filters.remove(FilterChip::Size(SizeRange::Over1Gb));
        // Removing a chip that is not the active one changes nothing.
        filters.remove(FilterChip::Modified(DateRange::Today));
        assert_eq!(
            filters.chips(),
            [
                FilterChip::Category(FileCategory::Videos),
                FilterChip::HiddenShown,
                FilterChip::Modified(DateRange::Last7Days)
            ]
        );
        filters.remove(FilterChip::HiddenShown);
        filters.remove(FilterChip::Modified(DateRange::Last7Days));
        filters.remove(FilterChip::Category(FileCategory::Videos));
        assert!(filters.is_default());
    }

    #[test]
    fn commands_drive_the_one_model() {
        let mut arrangement = Arrangement::default();
        arrangement.apply(ArrangeCommand::SortBy(SortKey::Modified));
        arrangement.apply(ArrangeCommand::SetDir(SortDir::Descending));
        arrangement.apply(ArrangeCommand::ToggleFoldersFirst);
        assert_eq!(
            arrangement.sort,
            SortSpec {
                key: SortKey::Modified,
                dir: SortDir::Descending,
                folders_first: false,
                natural: true
            }
        );
        arrangement.apply(ArrangeCommand::ToggleCategory(FileCategory::Audio));
        arrangement.apply(ArrangeCommand::ToggleDate(DateRange::Today));
        arrangement.apply(ArrangeCommand::ToggleDate(DateRange::Older));
        arrangement.apply(ArrangeCommand::ToggleSize(SizeRange::Over1Gb));
        arrangement.apply(ArrangeCommand::ToggleHidden);
        assert_eq!(arrangement.filters.modified, Some(DateRange::Older));
        assert!(!arrangement.filters.hide_hidden, "Ctrl+H shows them");
        assert_eq!(arrangement.filters.chips().len(), 4);
        arrangement.apply(ArrangeCommand::ToggleDate(DateRange::Older));
        assert_eq!(
            arrangement.filters.modified, None,
            "choosing it again clears it"
        );
        arrangement.apply(ArrangeCommand::ToggleCategory(FileCategory::Audio));
        arrangement.apply(ArrangeCommand::RemoveChip(FilterChip::Size(
            SizeRange::Over1Gb,
        )));
        arrangement.apply(ArrangeCommand::ToggleHidden);
        assert!(arrangement.filters.is_default());
        arrangement.apply(ArrangeCommand::ToggleHidden);
        arrangement.apply(ArrangeCommand::ToggleCategory(FileCategory::Code));
        arrangement.apply(ArrangeCommand::ClearFilters);
        assert!(arrangement.filters.is_default());
        assert_eq!(arrangement.sort.key, SortKey::Modified, "the sort is kept");
    }
}

/// Deterministic listings and the straightforward implementation the
/// index is proven equal to.
#[cfg(test)]
mod fixtures {
    use super::*;

    /// Hidden files shown, nothing else chosen: what the filter fixtures
    /// below were written against.
    pub(super) fn shown() -> Filters {
        Filters {
            hide_hidden: false,
            ..Filters::default()
        }
    }

    /// The old default: by name ascending, hidden files shown.
    pub(super) fn by_name() -> Arrangement {
        Arrangement {
            sort: SortSpec {
                key: SortKey::Name,
                dir: SortDir::Ascending,
                ..SortSpec::default()
            },
            filters: shown(),
        }
    }
    use chrono::FixedOffset;
    use fileflux::core::EntryKind;
    use fileflux::core::fs::filtered_indices;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    use std::path::Path;
    use std::time::Duration;

    pub(super) type Res<T> = Result<T, Box<dyn std::error::Error>>;

    /// A tiny xorshift64*: deterministic and dependency free.
    pub(super) struct Rng(u64);

    impl Rng {
        pub(super) fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }

        pub(super) fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        pub(super) fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound.max(1)
        }

        /// `true` one time in `n`.
        pub(super) fn one_in(&mut self, n: u64) -> bool {
            self.below(n) == 0
        }

        pub(super) fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
            let at = usize::try_from(self.below(items.len() as u64)).ok()?;
            items.get(at).cloned()
        }
    }

    /// 2026-11-18 (Wednesday) 12:00 in a UTC+2 zone.
    pub(super) fn now() -> Res<DateTime<FixedOffset>> {
        Ok(FixedOffset::east_opt(2 * 3_600)
            .ok_or("offset out of range")?
            .with_ymd_and_hms(2026, 11, 18, 12, 0, 0)
            .single()
            .ok_or("ambiguous test instant")?)
    }

    fn entry(
        name: OsString,
        kind: EntryKind,
        size: Option<u64>,
        modified: Option<SystemTime>,
    ) -> Entry {
        Entry {
            path: Path::new("/fx").join(&name),
            name,
            kind,
            is_symlink: false,
            size,
            modified,
            origin: None,
        }
    }

    const PIECES: &[&str] = &[
        "a", "A", "b", "B", "1", "01", "2", "10", "007", "é", "É", "İ", "Σ", "ς", ".", "_", " ",
        "IMG", "img", "ǅ", "ß",
    ];
    const EXTENSIONS: &[&str] = &[
        "", "txt", "PNG", "jpg", "mp4", "mp3", "pdf", "zip", "rs", "Rs", "toml", "bin", "K", "",
    ];
    /// Seconds before `now`: around every day boundary a date range has.
    const AGES: &[u64] = &[
        0,
        3_600,
        43_199,
        43_200,
        43_201,
        86_400,
        6 * 86_400 + 43_200,
        6 * 86_400 + 43_201,
        7 * 86_400,
        29 * 86_400 + 43_199,
        29 * 86_400 + 43_200,
        29 * 86_400 + 43_201,
        30 * 86_400,
        320 * 86_400 + 43_199,
        320 * 86_400 + 43_200,
        320 * 86_400 + 43_201,
        400 * 86_400,
    ];

    fn time(rng: &mut Rng, now: SystemTime) -> Option<SystemTime> {
        match rng.below(12) {
            0 => None,
            1 => Some(now + Duration::from_secs(86_400)),
            _ => Some(now - Duration::from_secs(rng.pick(AGES)?)),
        }
    }

    /// `count` entries built to collide: a tiny alphabet with case and digit
    /// variants, repeated names under different parents, non-UTF-8 names,
    /// every kind, and few distinct sizes and times, so every tie-break of
    /// the order is exercised.
    pub(super) fn tricky_listing(rng: &mut Rng, count: usize) -> Res<Vec<Entry>> {
        let epoch = SystemTime::from(now()?);
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let mut name = String::new();
            for _ in 0..=rng.below(4) {
                name.push_str(rng.pick(PIECES).unwrap_or("x"));
            }
            let extension = rng.pick(EXTENSIONS).unwrap_or_default();
            if !extension.is_empty() {
                name.push('.');
                name.push_str(extension);
            }
            let name = if rng.one_in(12) {
                let mut bytes = name.into_bytes();
                bytes.push(0xff);
                bytes.push(b'z');
                OsString::from_vec(bytes)
            } else {
                OsString::from(name)
            };
            let kind = match rng.below(10) {
                0..=2 => EntryKind::Directory,
                3 => EntryKind::Symlink,
                4 => EntryKind::Other,
                _ => EntryKind::File,
            };
            let size = match rng.below(8) {
                0 => None,
                1 => Some(1_000_000),
                2 => Some(100_000_000),
                3 => Some(999_999),
                4 => Some(2_000_000_000),
                _ => Some(rng.below(5)),
            };
            let mut one = entry(name, kind, size, time(rng, epoch));
            if rng.one_in(10) {
                // Repeated names live under another parent.
                one.path = Path::new("/other").join(&one.name);
            }
            entries.push(one);
        }
        Ok(entries)
    }

    /// Item counts for some of the folders of `entries`.
    pub(super) fn counts_for(rng: &mut Rng, entries: &[Entry]) -> FolderCounts {
        entries
            .iter()
            .filter(|entry| entry.is_directory())
            .filter_map(|entry| match rng.below(4) {
                0 => None,
                1 => Some((entry.path.clone(), None)),
                _ => Some((entry.path.clone(), Some(rng.below(3) as usize))),
            })
            .collect()
    }

    pub(super) fn random_arrangement(rng: &mut Rng) -> Arrangement {
        let mut arrangement = Arrangement {
            sort: SortSpec {
                key: rng.pick(&SortKey::ALL).unwrap_or(SortKey::Name),
                dir: if rng.one_in(2) {
                    SortDir::Ascending
                } else {
                    SortDir::Descending
                },
                folders_first: rng.one_in(2),
                natural: rng.one_in(2),
            },
            ..Arrangement::default()
        };
        let filters = &mut arrangement.filters;
        if rng.one_in(5) {
            filters.kinds.insert(FileCategory::Folders);
        } else {
            for category in FileCategory::ALL {
                if rng.one_in(6) {
                    filters.kinds.insert(category);
                }
            }
        }
        filters.hide_hidden = rng.one_in(3);
        if rng.one_in(2) {
            filters.modified = rng.pick(&DateRange::ALL);
        }
        if rng.one_in(3) {
            filters.size = rng.pick(&SizeRange::ALL);
        }
        arrangement
    }

    pub(super) fn random_query(rng: &mut Rng, entries: &[Entry]) -> String {
        match rng.below(6) {
            0 | 1 => String::new(),
            2 => rng.pick(PIECES).unwrap_or("a").to_owned(),
            3 => "\0".to_owned(),
            _ => {
                let name = rng
                    .pick(entries)
                    .map(|entry| entry.name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let chars: Vec<char> = name.chars().collect();
                let from = usize::try_from(rng.below(chars.len() as u64 + 1)).unwrap_or(0);
                let len = usize::try_from(rng.below(3) + 1).unwrap_or(1);
                chars.iter().skip(from).take(len).collect()
            }
        }
    }

    /// The straightforward implementation: filter, then stable-sort with
    /// [`compare_entries`], reading the entries every time.
    pub(super) fn reference<Tz: TimeZone>(
        arrangement: &Arrangement,
        entries: &[Entry],
        query: &str,
        counts: &FolderCounts,
        now: &DateTime<Tz>,
    ) -> Vec<usize> {
        let mut order: Vec<usize> = filtered_indices(entries, query)
            .into_iter()
            .filter(|&index| {
                entries
                    .get(index)
                    .is_some_and(|entry| arrangement.filters.admits(entry, now))
            })
            .collect();
        order.sort_by(|&a, &b| match (entries.get(a), entries.get(b)) {
            (Some(a), Some(b)) => compare_entries(a, b, arrangement.sort, counts),
            (a, b) => a.is_some().cmp(&b.is_some()).reverse(),
        });
        order
    }

    /// `count` pseudo-random entries for the timing runs: mixed case and
    /// digit runs in names, ~2 % folders, dotfiles, a few unknown sizes or
    /// times, `modified` spread over about three years. Names are unique.
    pub(super) fn large_listing(count: usize, seed: u64) -> Res<Vec<Entry>> {
        const STEMS: &[&str] = &[
            "IMG_", "img_", "Report ", "report-", "photo", "Éclair", "note", "a", "ab", "abc",
            "data", "ΣΑΣ", ".hidden", ".cache", "Makefile", "v", "x-", "backup.",
        ];
        const LARGE_EXTENSIONS: &[&str] = &[
            "", "txt", "PNG", "jpg", "mp4", "mp3", "pdf", "zip", "gz", "rs", "toml", "bin", "MD",
            "docx", "svg", "csv",
        ];
        let mut rng = Rng::new(seed);
        let epoch = SystemTime::from(now()?);
        Ok((0..count)
            .map(|serial| {
                let stem = rng.pick(STEMS).unwrap_or("f");
                let digits = match rng.below(4) {
                    0 => serial as u64,
                    1 => rng.below(1_000),
                    2 => rng.below(20),
                    _ => 0,
                };
                let is_dir = rng.below(50) == 0;
                let extension = rng.pick(LARGE_EXTENSIONS).unwrap_or_default();
                let mut name = format!("{stem}{digits}_{serial}");
                if !is_dir && !extension.is_empty() {
                    name.push('.');
                    name.push_str(extension);
                }
                let size = match rng.below(40) {
                    0 => None,
                    1 => Some(rng.below(3_000_000_000)),
                    _ => Some(rng.below(2_000_000)),
                };
                let modified = (rng.below(30) != 0).then(|| {
                    epoch - Duration::from_secs(rng.below(3 * 365 * 86_400))
                        + Duration::from_nanos(rng.below(1_000_000_000))
                });
                entry(
                    OsString::from(name),
                    if is_dir {
                        EntryKind::Directory
                    } else {
                        EntryKind::File
                    },
                    if is_dir { None } else { size },
                    modified,
                )
            })
            .collect())
    }
}

/// The optimized pipeline against the reference, on randomized listings.
#[cfg(test)]
mod equivalence {
    use std::path::Path;

    use super::fixtures::{
        Res, Rng, by_name, counts_for, now, random_arrangement, random_query, reference,
        tricky_listing,
    };
    use super::*;

    fn index_of(entries: &[Entry]) -> Arc<ArrangeIndex> {
        Arc::new(ArrangeIndex::build(entries))
    }

    #[test]
    fn every_sort_spec_matches_the_reference_on_colliding_listings() -> Res<()> {
        let at = now()?;
        for seed in 1..=6 {
            let mut rng = Rng::new(seed);
            let entries = tricky_listing(&mut rng, 260)?;
            let counts = counts_for(&mut rng, &entries);
            for key in SortKey::ALL {
                for dir in [SortDir::Ascending, SortDir::Descending] {
                    for folders_first in [true, false] {
                        for natural in [true, false] {
                            let arrangement = Arrangement {
                                sort: SortSpec {
                                    key,
                                    dir,
                                    folders_first,
                                    natural,
                                },
                                ..Arrangement::default()
                            };
                            for query in ["", "a", "1", "é"] {
                                assert_eq!(
                                    arrangement.arranged(&entries, query, &counts, &at),
                                    reference(&arrangement, &entries, query, &counts, &at),
                                    "seed {seed}, {arrangement:?}, query {query:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn random_filters_and_queries_match_the_reference() -> Res<()> {
        let at = now()?;
        for seed in 11..=16 {
            let mut rng = Rng::new(seed);
            let entries = tricky_listing(&mut rng, 300)?;
            let counts = counts_for(&mut rng, &entries);
            for _ in 0..120 {
                let arrangement = random_arrangement(&mut rng);
                let query = random_query(&mut rng, &entries);
                assert_eq!(
                    arrangement.arranged(&entries, &query, &counts, &at),
                    reference(&arrangement, &entries, &query, &counts, &at),
                    "seed {seed}, {arrangement:?}, query {query:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn a_listing_in_the_default_order_is_returned_as_the_reference_orders_it() -> Res<()> {
        // The scan sorts by name; the already-sorted fast path must give
        // the same answer as sorting.
        let at = now()?;
        let mut rng = Rng::new(99);
        let mut entries = tricky_listing(&mut rng, 400)?;
        fileflux::core::fs::sort_entries(&mut entries);
        let counts = FolderCounts::new();
        let arrangement = by_name();
        for query in ["", "a", "b1"] {
            assert_eq!(
                arrangement.arranged(&entries, query, &counts, &at),
                reference(&arrangement, &entries, query, &counts, &at)
            );
        }
        let unfiltered = arrangement.arranged(&entries, "", &counts, &at);
        assert_eq!(unfiltered, (0..entries.len()).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn date_windows_that_cannot_use_a_cutoff_keep_the_exact_test() -> Res<()> {
        // A zone with no midnight (or two) resolves to the per-entry test.
        let at = now()?;
        let mut rng = Rng::new(5);
        let entries = tricky_listing(&mut rng, 300)?;
        let counts = Arc::new(FolderCounts::new());
        let index = index_of(&entries);
        for range in DateRange::ALL {
            let arrangement = Arrangement {
                filters: Filters {
                    modified: Some(range),
                    ..Filters::default()
                },
                ..Arrangement::default()
            };
            let mut inputs = ArrangeInputs::new(
                0,
                arrangement.clone(),
                String::new(),
                Arc::clone(&counts),
                &at,
            );
            let exact = inputs.window.clone();
            inputs.window = Some(DateWindow {
                range,
                today: at.date_naive(),
                test: DateTest::PerEntry(Arc::new(move |time| range.matches(time, &at))),
            });
            assert_ne!(inputs.window, exact, "{range:?}: the test differs");
            let output = ArrangeJob::new(1, inputs, Arc::clone(&index), None).run();
            assert_eq!(
                *output.indices,
                reference(&arrangement, &entries, "", &counts, &at),
                "{range:?}"
            );
        }
        Ok(())
    }

    /// One random change to the inputs of a tab.
    fn mutate(
        rng: &mut Rng,
        entries: &[Entry],
        arrangement: &mut Arrangement,
        query: &mut String,
        counts: &mut Arc<FolderCounts>,
    ) {
        match rng.below(8) {
            0 | 1 => query.push_str(&random_query(rng, entries)),
            2 => {
                query.pop();
            }
            3 => query.clear(),
            4 => *counts = Arc::new(counts_for(rng, entries)),
            5 => *arrangement = random_arrangement(rng),
            _ => {
                let command = match rng.below(9) {
                    0 => ArrangeCommand::SortBy(rng.pick(&SortKey::ALL).unwrap_or(SortKey::Name)),
                    1 => ArrangeCommand::ToggleFoldersFirst,
                    2 => ArrangeCommand::ToggleNatural,
                    3 => ArrangeCommand::ToggleHidden,
                    4 => ArrangeCommand::ToggleCategory(
                        rng.pick(&FileCategory::ALL).unwrap_or(FileCategory::Other),
                    ),
                    5 => ArrangeCommand::ToggleDate(
                        rng.pick(&DateRange::ALL).unwrap_or(DateRange::Today),
                    ),
                    6 => ArrangeCommand::ToggleSize(
                        rng.pick(&SizeRange::ALL).unwrap_or(SizeRange::Under1Mb),
                    ),
                    7 => ArrangeCommand::SetDir(SortDir::Descending),
                    _ => ArrangeCommand::ClearFilters,
                };
                arrangement.apply(command);
            }
        }
    }

    #[test]
    fn reusing_partial_results_never_changes_the_answer() -> Res<()> {
        // A typing-and-toggling session over one listing, through the
        // cache: the sort, the filter pass and the narrowed query hits are
        // reused whenever their inputs still match, and every step must
        // equal the reference computed from scratch.
        let at = now()?;
        for seed in 21..=26 {
            let mut rng = Rng::new(seed);
            let entries = tricky_listing(&mut rng, 300)?;
            let mut counts = Arc::new(counts_for(&mut rng, &entries));
            let mut arrangement = Arrangement::default();
            let mut query = String::new();
            let mut cache = ArrangeCache::default();
            for step in 0..160 {
                mutate(
                    &mut rng,
                    &entries,
                    &mut arrangement,
                    &mut query,
                    &mut counts,
                );
                let inputs = ArrangeInputs::new(
                    7,
                    arrangement.clone(),
                    query.clone(),
                    Arc::clone(&counts),
                    &at,
                );
                let expected = reference(&arrangement, &entries, &query, &counts, &at);
                let shown = if step % 2 == 0 {
                    cache.resolve(&inputs, &entries)
                } else {
                    match cache.begin(inputs.clone(), &entries) {
                        Some(job) => {
                            let output = job.run();
                            assert!(cache.commit(output, &inputs), "seed {seed} step {step}");
                            cache.resolve(&inputs, &entries)
                        }
                        None => cache.resolve(&inputs, &entries),
                    }
                };
                assert_eq!(
                    *shown, expected,
                    "seed {seed}, step {step}, {arrangement:?}, query {query:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn numbers_of_every_shape_order_like_the_reference() -> Res<()> {
        // Leading zeros, equal values, numbers beside letters, a number
        // longer than the sort prefix, and one too long to be sized.
        let long = "9".repeat(40);
        let longer = "9".repeat(300);
        let longest = format!("{}8", "9".repeat(300));
        let names: Vec<String> = [
            "img1",
            "img12",
            "img13",
            "img21",
            "img019",
            "img01",
            "img001",
            "img10",
            "img9",
            "img1a",
            "img1_",
            "img",
            "imga",
            "img 1",
            "0",
            "00",
            "000",
            "1",
            "a1b2c3d4e5f6g7h8i9j10",
            "a1b2c3d4e5f6g7h8i9j9",
            "abcdefgh1",
            "abcdefgh10",
            "abcdefgh2",
            "abcdefgh",
            "abcdefghi",
            "ABCDEFGH1",
            "z9",
            "z10",
            "z09",
        ]
        .iter()
        .map(|name| (*name).to_owned())
        .chain([
            "x".to_owned() + &long,
            "x".to_owned() + &longer,
            "x".to_owned() + &longest,
        ])
        .chain([
            format!("x{long}1"),
            format!("x{longer}1"),
            "xé1".to_owned(),
            "x1é".to_owned(),
        ])
        .collect();
        let entries: Vec<Entry> = names
            .iter()
            .map(|name| Entry {
                name: name.into(),
                path: Path::new("/fx").join(name),
                kind: fileflux::core::EntryKind::File,
                is_symlink: false,
                size: Some(1),
                modified: None,
                origin: None,
            })
            .collect();
        let at = now()?;
        let counts = FolderCounts::new();
        for natural in [true, false] {
            for dir in [SortDir::Ascending, SortDir::Descending] {
                for key in SortKey::ALL {
                    let arrangement = Arrangement {
                        sort: SortSpec {
                            key,
                            dir,
                            folders_first: true,
                            natural,
                        },
                        ..Arrangement::default()
                    };
                    assert_eq!(
                        arrangement.arranged(&entries, "", &counts, &at),
                        reference(&arrangement, &entries, "", &counts, &at),
                        "{arrangement:?}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn the_name_scan_finds_every_match_once_in_listing_order() -> Res<()> {
        let mut rng = Rng::new(3);
        let entries = tricky_listing(&mut rng, 500)?;
        let index = ArrangeIndex::build(&entries);
        for needle in [
            "a", "b", "1", "é", "i̇", "σ", "ς", "img", "a_", ".", "zz", "b.", "\0",
        ] {
            let expected: Vec<usize> = fileflux::core::fs::filtered_indices(&entries, needle);
            assert_eq!(index.query_hits(needle, None), expected, "{needle:?}");
        }
        // Narrowing from a shorter query's hits gives the same set.
        let previous = QueryHits {
            needle: "a".to_owned(),
            hits: index.query_hits("a", None),
        };
        assert_eq!(
            index.query_hits("a1", Some(&previous)),
            index.query_hits("a1", None)
        );
        // A query that does not extend the previous one ignores its hits.
        assert_eq!(
            index.query_hits("b", Some(&previous)),
            index.query_hits("b", None)
        );
        Ok(())
    }
}

/// The generation guard around the pipeline.
#[cfg(test)]
mod guard {
    use super::fixtures::{Res, Rng, by_name, now, tricky_listing};
    use super::*;

    fn inputs(
        epoch: u64,
        query: &str,
        counts: &Arc<FolderCounts>,
        at: &DateTime<chrono::FixedOffset>,
    ) -> ArrangeInputs {
        ArrangeInputs::new(epoch, by_name(), query.to_owned(), Arc::clone(counts), at)
    }

    #[test]
    fn the_newest_job_for_the_current_inputs_is_shown() -> Res<()> {
        let at = now()?;
        let entries = tricky_listing(&mut Rng::new(1), 50)?;
        let counts = Arc::new(FolderCounts::new());
        let mut cache = ArrangeCache::default();
        let current = inputs(1, "a", &counts, &at);
        let job = cache.begin(current.clone(), &entries).ok_or("no job")?;
        assert!(cache.is_pending());
        assert_eq!(job.generation(), 1);
        let output = job.run();
        assert_eq!(output.generation(), 1);
        assert!(cache.commit(output, &current));
        assert!(!cache.is_pending());
        assert!(
            cache.begin(current, &entries).is_none(),
            "the shown result is already current"
        );
        Ok(())
    }

    #[test]
    fn an_older_job_is_dropped_when_a_newer_one_began() -> Res<()> {
        let at = now()?;
        let entries = tricky_listing(&mut Rng::new(2), 80)?;
        let counts = Arc::new(FolderCounts::new());
        let mut cache = ArrangeCache::default();
        let first = inputs(1, "a", &counts, &at);
        let second = inputs(1, "ab", &counts, &at);
        let slow = cache.begin(first.clone(), &entries).ok_or("no job")?;
        let fast = cache.begin(second.clone(), &entries).ok_or("no job")?;
        assert_eq!(fast.generation(), 2);
        // The newer job lands first, then the stale one arrives late.
        assert!(cache.commit(fast.run(), &second));
        let shown = cache.resolve(&second, &entries);
        assert!(!cache.commit(slow.run(), &second), "stale generation");
        assert_eq!(cache.resolve(&second, &entries), shown, "nothing changed");
        Ok(())
    }

    #[test]
    fn a_result_for_inputs_that_moved_on_is_not_shown_and_the_next_job_starts() -> Res<()> {
        let at = now()?;
        let entries = tricky_listing(&mut Rng::new(3), 80)?;
        let counts = Arc::new(FolderCounts::new());
        let mut cache = ArrangeCache::default();
        let typed = inputs(1, "a", &counts, &at);
        let job = cache.begin(typed, &entries).ok_or("no job")?;
        let moved_on = inputs(1, "ab", &counts, &at);
        assert!(!cache.commit(job.run(), &moved_on), "inputs changed");
        assert!(!cache.is_pending(), "the stale job no longer blocks");
        assert!(cache.begin(moved_on, &entries).is_some());
        Ok(())
    }

    #[test]
    fn a_new_listing_refuses_the_job_of_the_old_one() -> Res<()> {
        let at = now()?;
        let old = tricky_listing(&mut Rng::new(4), 60)?;
        let new = tricky_listing(&mut Rng::new(5), 30)?;
        let counts = Arc::new(FolderCounts::new());
        let mut cache = ArrangeCache::default();
        let before = inputs(1, "", &counts, &at);
        let job = cache.begin(before, &old).ok_or("no job")?.run();
        cache.reset(2, None);
        let after = inputs(2, "", &counts, &at);
        assert!(!cache.commit(job, &after), "another listing");
        let fresh = cache.begin(after.clone(), &new).ok_or("no job")?;
        assert!(cache.commit(fresh.run(), &after));
        assert_eq!(cache.resolve(&after, &new).len(), new.len());
        Ok(())
    }

    #[test]
    fn while_a_job_runs_the_last_result_of_the_listing_stays_and_a_fresh_one_is_empty() -> Res<()> {
        let at = now()?;
        let entries = tricky_listing(&mut Rng::new(6), 60)?;
        let counts = Arc::new(FolderCounts::new());
        let mut cache = ArrangeCache::default();
        let all = inputs(1, "", &counts, &at);
        let shown = cache.resolve(&all, &entries);
        assert_eq!(shown.len(), entries.len());
        let typed = inputs(1, "zzz-no-match", &counts, &at);
        let _running = cache.begin(typed.clone(), &entries).ok_or("no job")?;
        assert_eq!(
            cache.resolve(&typed, &entries),
            shown,
            "the previous result stays until the job lands"
        );
        assert!(
            cache.begin(typed, &entries).is_none(),
            "a job for these inputs is already running"
        );
        cache.reset(2, None);
        let fresh = inputs(2, "", &counts, &at);
        let _running = cache.begin(fresh.clone(), &entries).ok_or("no job")?;
        assert!(
            cache.resolve(&fresh, &entries).is_empty(),
            "no result of the new listing yet, and nothing computed on this thread"
        );
        Ok(())
    }

    #[test]
    fn cancelling_lets_the_same_inputs_start_again() -> Res<()> {
        let at = now()?;
        let entries = tricky_listing(&mut Rng::new(7), 40)?;
        let counts = Arc::new(FolderCounts::new());
        let mut cache = ArrangeCache::default();
        let wanted = inputs(1, "a", &counts, &at);
        let _dropped = cache.begin(wanted.clone(), &entries).ok_or("no job")?;
        assert!(cache.begin(wanted.clone(), &entries).is_none());
        cache.cancel();
        assert!(cache.begin(wanted, &entries).is_some());
        Ok(())
    }

    #[test]
    fn new_folder_counts_make_the_shown_result_stale() -> Res<()> {
        let at = now()?;
        let entries = tricky_listing(&mut Rng::new(8), 40)?;
        let counts = Arc::new(FolderCounts::new());
        let mut cache = ArrangeCache::default();
        let _ = cache.resolve(&inputs(1, "", &counts, &at), &entries);
        let counted = Arc::new(FolderCounts::new());
        assert!(
            cache
                .begin(inputs(1, "", &counted, &at), &entries)
                .is_some(),
            "replaced counts are a new input"
        );
        Ok(())
    }

    #[test]
    fn the_clock_only_matters_with_a_date_filter() -> Res<()> {
        let at = now()?;
        let later = at + chrono::Duration::hours(30);
        let counts = Arc::new(FolderCounts::new());
        assert_eq!(
            inputs(1, "", &counts, &at),
            inputs(1, "", &counts, &later),
            "no date filter, no dependence on the time"
        );
        let dated = |when| {
            ArrangeInputs::new(
                1,
                Arrangement {
                    filters: Filters {
                        modified: Some(DateRange::Today),
                        ..Filters::default()
                    },
                    ..Arrangement::default()
                },
                String::new(),
                Arc::clone(&counts),
                when,
            )
        };
        assert_eq!(dated(&at), dated(&at));
        assert_ne!(dated(&at), dated(&later), "another day, another window");
        Ok(())
    }
}

/// Timings on large synthetic listings, `#[ignore]`d: run with
/// `cargo test --release --offline arrange::perf -- --ignored --nocapture`.
/// `FILEFLUX_PERF_ENTRIES` sets the size (default 1,000,000),
/// `FILEFLUX_PERF_RUNS` the repetitions (best of, default 3) and
/// `FILEFLUX_PERF_REFERENCE=1` times the straightforward implementation
/// instead of the index.
#[cfg(test)]
mod perf {
    use super::fixtures::{Res, Rng, large_listing, now, reference};
    use super::*;
    use std::time::{Duration, Instant};

    fn env_number(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    fn millis(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1_000.0
    }

    /// Best of `runs` timed calls of `work`, in milliseconds, with the
    /// number of entries the last call produced.
    fn best(runs: usize, mut work: impl FnMut() -> usize) -> (f64, usize) {
        let mut best = Duration::MAX;
        let mut shown = 0;
        for _ in 0..runs {
            let started = Instant::now();
            shown = work();
            best = best.min(started.elapsed());
        }
        (millis(best), shown)
    }

    #[test]
    #[ignore = "timing run: cargo test --release --offline arrange::perf -- --ignored --nocapture"]
    fn large_folder_timings() -> Res<()> {
        let count = env_number("FILEFLUX_PERF_ENTRIES", 1_000_000);
        let runs = env_number("FILEFLUX_PERF_RUNS", 3);
        let against_reference = env_number("FILEFLUX_PERF_REFERENCE", 0) != 0;
        let mut entries = large_listing(count, 0xF11E_F10C)?;
        // The scan hands the listing over sorted by name.
        fileflux::core::fs::sort_entries(&mut entries);
        let mut rng = Rng::new(7);
        let counts: FolderCounts = entries
            .iter()
            .filter(|entry| entry.is_directory())
            .filter_map(|entry| match rng.below(4) {
                0 => None,
                1 => Some((entry.path.clone(), None)),
                _ => Some((entry.path.clone(), Some(rng.below(500) as usize))),
            })
            .collect();
        let counts = Arc::new(counts);
        let at = now()?;
        println!(
            "entries: {count}, profile: {}, implementation: {}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            if against_reference {
                "reference"
            } else {
                "index"
            }
        );
        let report = |label: &str, (time, shown): (f64, usize)| {
            println!("{label:<36} {time:>10.1} ms  ({shown} shown)");
        };

        let started = Instant::now();
        let index = Arc::new(ArrangeIndex::build(&entries));
        if !against_reference {
            println!(
                "{:<36} {:>10.1} ms",
                "index build (once per listing)",
                millis(started.elapsed())
            );
        }
        let inputs = |arrangement: &Arrangement, query: &str| {
            ArrangeInputs::new(
                1,
                arrangement.clone(),
                query.to_owned(),
                Arc::clone(&counts),
                &at,
            )
        };
        // One arrangement, as the UI would run it with `warm` at hand.
        let arrange = |arrangement: &Arrangement, query: &str, warm: &Warm| {
            if against_reference {
                let shown = reference(arrangement, &entries, query, &counts, &at);
                ArrangeOutput {
                    generation: 1,
                    inputs: inputs(arrangement, query),
                    indices: Arc::new(shown),
                    warm: warm.clone(),
                }
            } else {
                ArrangeJob::new(
                    1,
                    inputs(arrangement, query),
                    Arc::clone(&index),
                    Some(warm),
                )
                .run()
            }
        };
        let measure = |arrangement: &Arrangement, query: &str, warm: &Warm| {
            best(runs, || arrange(arrangement, query, warm).indices.len())
        };
        let cold = Warm {
            epoch: 1,
            ..Warm::default()
        };

        let default = Arrangement::default();
        report(
            "initial sort (name, no query)",
            measure(&default, "", &cold),
        );
        let base = arrange(&default, "", &cold);
        // Typing: each keystroke starts from the previous one's results.
        let mut warm = base.warm.clone();
        for query in ["a", "ab", "abc"] {
            report(
                &format!("keystroke query {query:?}"),
                measure(&default, query, &warm),
            );
            warm = arrange(&default, query, &warm).warm;
        }
        report(
            "query \"a\" without narrowing",
            measure(&default, "a", &base.warm),
        );
        // Toggles keep the sort of the listing and recompute the filter.
        let toggled = |command: ArrangeCommand| {
            let mut arrangement = Arrangement::default();
            arrangement.apply(command);
            arrangement
        };
        for (label, command) in [
            ("toggle hide hidden", ArrangeCommand::ToggleHidden),
            (
                "toggle category Images",
                ArrangeCommand::ToggleCategory(FileCategory::Images),
            ),
            (
                "toggle category Folders",
                ArrangeCommand::ToggleCategory(FileCategory::Folders),
            ),
            (
                "toggle date Last 7 days",
                ArrangeCommand::ToggleDate(DateRange::Last7Days),
            ),
            (
                "toggle size < 1 MB",
                ArrangeCommand::ToggleSize(SizeRange::Under1Mb),
            ),
        ] {
            report(label, measure(&toggled(command), "", &base.warm));
        }
        // A sort change sorts again; only the filter pass is kept.
        let resort = Warm {
            sorted: None,
            ..base.warm.clone()
        };
        for (label, command) in [
            (
                "toggle folders first (off)",
                ArrangeCommand::ToggleFoldersFirst,
            ),
            ("toggle natural (off)", ArrangeCommand::ToggleNatural),
            (
                "sort by Name, descending",
                ArrangeCommand::SetDir(SortDir::Descending),
            ),
            ("sort by Size", ArrangeCommand::SortBy(SortKey::Size)),
            (
                "sort by Modified",
                ArrangeCommand::SortBy(SortKey::Modified),
            ),
            ("sort by Type", ArrangeCommand::SortBy(SortKey::Type)),
        ] {
            report(label, measure(&toggled(command), "", &resort));
        }
        // Going back to a key already sorted by is a toggle again.
        let sorted_by_size = arrange(&toggled(ArrangeCommand::SortBy(SortKey::Size)), "", &cold);
        report(
            "sort by Size again (cached)",
            measure(
                &toggled(ArrangeCommand::SortBy(SortKey::Size)),
                "",
                &sorted_by_size.warm,
            ),
        );
        Ok(())
    }
}
