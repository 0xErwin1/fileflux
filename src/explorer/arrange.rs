//! How a tab arranges its folder: the sort and the filters, as plain data.
//!
//! Nothing here touches GPUI, the filesystem or the clock (callers pass
//! `now`), and every type derives `Default` and `Clone`/`Eq`, so the whole
//! [`Arrangement`] can later be persisted to a settings file as it is. The
//! grid and the list read the one order [`Arrangement::arranged`] returns.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Datelike as _, TimeZone, Utc};
use fileflux::core::Entry;
use fileflux::core::fs::{filtered_indices, natural_cmp};
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
            key: SortKey::Name,
            dir: SortDir::Ascending,
            folders_first: true,
            natural: true,
        }
    }
}

impl SortSpec {
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

/// Orders two entries under `spec`. With folders first, folders precede
/// files whatever the key or direction; within a group the key decides (a
/// missing size or time sorts lowest) and the direction reverses only that.
/// Ties fall back to the name ascending and then the raw path, so the
/// order is total and stable.
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

fn name_cmp(a: &Entry, b: &Entry, natural: bool) -> Ordering {
    if natural {
        natural_cmp(&a.name, &b.name)
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
    /// Hidden (dot) files are hidden.
    NoHidden,
    Modified(DateRange),
    Size(SizeRange),
}

impl FilterChip {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Category(category) => category.label(),
            Self::NoHidden => "No hidden files",
            Self::Modified(range) => range.label(),
            Self::Size(range) => range.label(),
        }
    }
}

/// The filters of a tab. All of them combine with AND, and with the name
/// search. The default shows everything, hidden files included.
///
/// Folders have no byte size, so a size range never applies to them. A date
/// range applies to folders only when `Folders` is the one chip selected;
/// otherwise folders pass it.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub(crate) struct Filters {
    /// The selected categories; none selected means every category.
    pub(crate) kinds: BTreeSet<FileCategory>,
    /// Hide names starting with a dot.
    pub(crate) hide_hidden: bool,
    pub(crate) modified: Option<DateRange>,
    pub(crate) size: Option<SizeRange>,
}

impl Filters {
    /// Nothing filters.
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The active filters, in chip order.
    pub(crate) fn chips(&self) -> Vec<FilterChip> {
        self.kinds
            .iter()
            .copied()
            .map(FilterChip::Category)
            .chain(self.hide_hidden.then_some(FilterChip::NoHidden))
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
            FilterChip::NoHidden => self.hide_hidden = false,
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

    /// Whether `entry` passes every filter at `now`.
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
    /// filters, in sorted order.
    pub(crate) fn arranged<Tz: TimeZone>(
        &self,
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
                    .is_some_and(|entry| self.filters.admits(entry, now))
            })
            .collect();
        order.sort_by(|&a, &b| match (entries.get(a), entries.get(b)) {
            (Some(a), Some(b)) => compare_entries(a, b, self.sort, counts),
            (a, b) => a.is_some().cmp(&b.is_some()).reverse(),
        });
        order
    }
}

#[cfg(test)]
mod tests {
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
            folders_first: false,
            natural: false,
            ..SortSpec::default()
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
    fn the_default_arrangement_shows_everything_by_natural_name() -> TestResult {
        let arrangement = Arrangement::default();
        assert!(arrangement.filters.is_default());
        assert!(arrangement.sort.folders_first && arrangement.sort.natural);
        assert!(!arrangement.filters.hide_hidden, "hidden files show today");
        assert!(arrangement.filters.chips().is_empty());
        let list = fixture()?;
        assert_eq!(arranged(&list, &arrangement, "")?.len(), list.len());
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
            ..Arrangement::default()
        }
    }

    #[test]
    fn each_filter_alone() -> TestResult {
        let list = mixed()?;
        let kinds = |set: &[FileCategory]| {
            with(Filters {
                kinds: set.iter().copied().collect(),
                ..Filters::default()
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
                    ..Filters::default()
                }),
                ""
            )?,
            ["pics", "a.png", "b.PNG", "c.mp4", "d.zip", "e.rs"]
        );
        let date = |range| {
            with(Filters {
                modified: Some(range),
                ..Filters::default()
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
                ..Filters::default()
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
            ..Filters::default()
        });
        assert_eq!(arranged(&list, &only_folders, "")?, ["pics"]);
        // A byte-size range never applies to folders, even then.
        let only_folders_sized = with(Filters {
            kinds: BTreeSet::from([FileCategory::Folders]),
            size: Some(SizeRange::Over1Gb),
            ..Filters::default()
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
            ..Filters::default()
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
                ..Filters::default()
            },
            Filters {
                size: Some(SizeRange::Under1Mb),
                ..Filters::default()
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
            hide_hidden: true,
            modified: Some(DateRange::Last7Days),
            size: Some(SizeRange::Over1Gb),
        };
        let labels: Vec<&str> = filters.chips().into_iter().map(FilterChip::label).collect();
        assert_eq!(
            labels,
            [
                "Images",
                "Videos",
                "No hidden files",
                "Last 7 days",
                "> 1 GB"
            ]
        );
        filters.remove(FilterChip::Category(FileCategory::Images));
        filters.remove(FilterChip::Size(SizeRange::Over1Gb));
        // Removing a chip that is not the active one changes nothing.
        filters.remove(FilterChip::Modified(DateRange::Today));
        assert_eq!(
            filters.chips(),
            [
                FilterChip::Category(FileCategory::Videos),
                FilterChip::NoHidden,
                FilterChip::Modified(DateRange::Last7Days)
            ]
        );
        filters.remove(FilterChip::NoHidden);
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
        assert!(arrangement.filters.hide_hidden);
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
