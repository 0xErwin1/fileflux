//! Display helpers for the explorer: honest metadata formatting. Sizes and
//! times come straight from the scan; nothing is invented for missing
//! metadata. Byte sizes come from the one SI formatter,
//! [`fileflux::core::places::format_bytes`], shared by the grid, the list,
//! the status bar and the sidebar.

use std::time::SystemTime;

use chrono::{DateTime, Datelike as _, TimeZone, Utc};
use fileflux::core::places::format_bytes;

/// The Size cell of the details list: folders show their item count when
/// the caller knows it (`child_count`) and stay empty otherwise — the scan
/// carries no child counts and the UI thread never walks directories to
/// find them. Everything else keeps its human size.
pub(crate) fn format_entry_size(
    entry: &fileflux::core::Entry,
    child_count: Option<usize>,
) -> String {
    if entry.is_directory() {
        child_count.map(format_item_count).unwrap_or_default()
    } else {
        entry
            .size
            .map_or_else(|| "\u{2014}".to_owned(), format_bytes)
    }
}

/// `"1 item"` / `"12 items"`.
pub(crate) fn format_item_count(count: usize) -> String {
    if count == 1 {
        "1 item".to_owned()
    } else {
        format!("{count} items")
    }
}

/// The modification time relative to `now`, in `now`'s time zone: `Today,
/// 09:41` for the same calendar day, `Yesterday`, `N days ago` up to six
/// days back, then `18 Nov` within `now`'s year and `18 Nov 2025` before it.
/// A time later than the end of today is not "relative" and reads as a plain
/// date. `None` means the scan could not read the time and renders as an em
/// dash. Pure over `now`, so every branch is testable deterministically.
pub(crate) fn format_modified<Tz: TimeZone>(
    time: Option<SystemTime>,
    now: &DateTime<Tz>,
) -> String {
    let Some(time) = time else {
        return "\u{2014}".to_owned();
    };
    let local = DateTime::<Utc>::from(time)
        .with_timezone(&now.timezone())
        .naive_local();
    let days_ago = now
        .date_naive()
        .signed_duration_since(local.date())
        .num_days();
    match days_ago {
        0 => format!("Today, {}", local.format("%H:%M")),
        1 => "Yesterday".to_owned(),
        2..=6 => format!("{days_ago} days ago"),
        _ if local.year() == now.year() => local.format("%-d %b").to_string(),
        _ => local.format("%-d %b %Y").to_string(),
    }
}

/// What a breadcrumb draws before its label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrumbKind {
    /// The home directory: house icon plus "Home".
    Home,
    /// The filesystem root: "/".
    Root,
    /// A folder segment; the last one is the current folder.
    Folder,
    /// Collapsed leading segments; navigates to the nearest hidden parent.
    Ellipsis,
}

/// One clickable breadcrumb segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Crumb {
    pub(crate) label: String,
    pub(crate) kind: CrumbKind,
    pub(crate) target: fileflux::core::Location,
}

/// Folder segments kept after the root crumb before the leading ones
/// collapse into an ellipsis, so the current folder always stays visible.
const MAX_FOLDER_CRUMBS: usize = 4;

/// The breadcrumbs of `location`: `Home` when under `home`, else `/`, then
/// one segment per folder, collapsing the leading folders into an ellipsis
/// beyond [`MAX_FOLDER_CRUMBS`]. The Trash is one segment. Targets keep the
/// raw path components; labels are lossy.
pub(crate) fn breadcrumbs(
    location: &fileflux::core::Location,
    home: Option<&std::path::Path>,
) -> Vec<Crumb> {
    use fileflux::core::Location;
    use std::path::Component;

    let path = match location {
        Location::Trash => {
            return vec![Crumb {
                label: "Home Trash".to_owned(),
                kind: CrumbKind::Folder,
                target: Location::Trash,
            }];
        }
        Location::Directory(path) => path,
    };
    let (base, root_kind, rest) = match home.filter(|home| path.starts_with(home)) {
        Some(home) => (
            home.to_path_buf(),
            CrumbKind::Home,
            path.strip_prefix(home).unwrap_or(path),
        ),
        None => (
            std::path::PathBuf::from("/"),
            CrumbKind::Root,
            path.strip_prefix("/").unwrap_or(path),
        ),
    };
    let root_target = Location::Directory(base.clone());
    let mut crumbs = vec![Crumb {
        label: if root_kind == CrumbKind::Home {
            "Home"
        } else {
            "/"
        }
        .to_owned(),
        kind: root_kind,
        target: root_target.clone(),
    }];
    let mut walked = base;
    let mut folders = Vec::new();
    for component in rest.components() {
        if let Component::Normal(name) = component {
            walked.push(name);
            folders.push(Crumb {
                label: name.to_string_lossy().into_owned(),
                kind: CrumbKind::Folder,
                target: Location::Directory(walked.clone()),
            });
        }
    }
    if folders.len() > MAX_FOLDER_CRUMBS {
        let hidden = folders.len() - MAX_FOLDER_CRUMBS;
        let parent = hidden
            .checked_sub(1)
            .and_then(|index| folders.get(index))
            .map_or_else(|| root_target.clone(), |crumb| crumb.target.clone());
        folders.drain(..hidden);
        crumbs.push(Crumb {
            label: "\u{2026}".to_owned(),
            kind: CrumbKind::Ellipsis,
            target: parent,
        });
    }
    crumbs.extend(folders);
    crumbs
}

/// `"1 folder"` / `"4 folders"`.
fn count_of(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// The subbar meta of a listing: `"4 folders · 7 files"`, a part omitted
/// when it is zero, `"empty"` for nothing.
pub(crate) fn folder_counts_label(entries: &[fileflux::core::Entry]) -> String {
    let folders = entries.iter().filter(|entry| entry.is_directory()).count();
    let files = entries.len() - folders;
    let parts: Vec<String> = [(folders, "folder", "folders"), (files, "file", "files")]
        .into_iter()
        .filter(|(count, ..)| *count > 0)
        .map(|(count, singular, plural)| count_of(count, singular, plural))
        .collect();
    if parts.is_empty() {
        "empty".to_owned()
    } else {
        parts.join(" \u{b7} ")
    }
}

/// The status bar's left text: `"N items"` (or `"n of N shown"` while a
/// query filters), and `"N selected · <size>"` once something is selected.
pub(crate) fn status_label(
    total: usize,
    shown: Option<usize>,
    selected: usize,
    selected_bytes: u64,
) -> String {
    if selected > 0 {
        return format!(
            "{selected} selected \u{b7} {}",
            format_bytes(selected_bytes)
        );
    }
    match shown {
        Some(shown) => format!("{shown} of {total} shown"),
        None => count_of(total, "item", "items"),
    }
}

/// The visible position of index `index` in a chunked grid row of `columns`
/// tiles: row = position / columns (floored).
pub(crate) fn grid_row(position: usize, columns: usize) -> usize {
    // No columns means no chunking; the position stays on row zero.
    position.checked_div(columns).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use fileflux::core::EntryKind;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// 2026-11-18 (Wednesday) 12:00 in a UTC+2 zone.
    fn now() -> Result<DateTime<FixedOffset>, Box<dyn std::error::Error>> {
        Ok(zone()?
            .with_ymd_and_hms(2026, 11, 18, 12, 0, 0)
            .single()
            .ok_or("ambiguous test instant")?)
    }

    fn zone() -> Result<FixedOffset, Box<dyn std::error::Error>> {
        FixedOffset::east_opt(2 * 3_600).ok_or_else(|| "offset out of range".into())
    }

    /// A local (UTC+2) wall-clock instant as a `SystemTime`.
    fn at(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
    ) -> Result<Option<SystemTime>, Box<dyn std::error::Error>> {
        let local = zone()?
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .ok_or("ambiguous test instant")?;
        Ok(Some(local.into()))
    }

    #[test]
    fn modified_reads_relative_for_the_last_week() -> TestResult {
        let now = now()?;
        assert_eq!(
            format_modified(at(2026, 11, 18, 9, 41)?, &now),
            "Today, 09:41"
        );
        assert_eq!(
            format_modified(at(2026, 11, 18, 0, 0)?, &now),
            "Today, 00:00",
            "the start of the local day is still today"
        );
        assert_eq!(
            format_modified(at(2026, 11, 17, 23, 59)?, &now),
            "Yesterday",
            "one minute before local midnight is yesterday"
        );
        assert_eq!(format_modified(at(2026, 11, 17, 0, 0)?, &now), "Yesterday");
        assert_eq!(
            format_modified(at(2026, 11, 16, 23, 59)?, &now),
            "2 days ago"
        );
        assert_eq!(
            format_modified(at(2026, 11, 12, 8, 0)?, &now),
            "6 days ago",
            "six days is the last relative day"
        );
        Ok(())
    }

    #[test]
    fn modified_reads_as_a_date_from_seven_days_back() -> TestResult {
        let now = now()?;
        assert_eq!(
            format_modified(at(2026, 11, 11, 23, 0)?, &now),
            "11 Nov",
            "seven days ago is the first absolute date"
        );
        assert_eq!(format_modified(at(2026, 1, 5, 10, 0)?, &now), "5 Jan");
        assert_eq!(
            format_modified(at(2025, 11, 18, 10, 0)?, &now),
            "18 Nov 2025",
            "an older year names the year"
        );
        assert_eq!(
            format_modified(at(2025, 12, 31, 23, 30)?, &now),
            "31 Dec 2025"
        );
        Ok(())
    }

    #[test]
    fn modified_uses_the_local_calendar_day_not_utc() -> TestResult {
        // 23:30 UTC on the 17th is 01:30 on the 18th at UTC+2: still today.
        let now = now()?;
        let utc_evening = Utc
            .with_ymd_and_hms(2026, 11, 17, 23, 30, 0)
            .single()
            .ok_or("ambiguous test instant")?;
        assert_eq!(
            format_modified(Some(utc_evening.into()), &now),
            "Today, 01:30"
        );
        Ok(())
    }

    #[test]
    fn modified_handles_unknown_and_future_times() -> TestResult {
        let now = now()?;
        assert_eq!(format_modified(None, &now), "\u{2014}");
        assert_eq!(
            format_modified(at(2026, 11, 18, 23, 0)?, &now),
            "Today, 23:00",
            "later today is still today"
        );
        assert_eq!(
            format_modified(at(2026, 11, 20, 9, 0)?, &now),
            "20 Nov",
            "a future day falls back to a plain date"
        );
        assert_eq!(
            format_modified(Some(std::time::UNIX_EPOCH), &now),
            "1 Jan 1970"
        );
        Ok(())
    }

    #[test]
    fn folders_show_item_counts_and_files_keep_sizes() {
        use fileflux::core::Entry;
        use std::ffi::OsString;
        let mut entry = Entry {
            name: OsString::from("docs"),
            path: "/docs".into(),
            kind: EntryKind::Directory,
            is_symlink: false,
            size: None,
            modified: None,
            origin: None,
        };
        assert_eq!(format_entry_size(&entry, Some(12)), "12 items");
        assert_eq!(format_entry_size(&entry, Some(1)), "1 item");
        assert_eq!(format_entry_size(&entry, Some(0)), "0 items");
        assert_eq!(format_entry_size(&entry, None), "", "unknown stays empty");
        entry.kind = EntryKind::File;
        entry.size = Some(2_048);
        assert_eq!(format_entry_size(&entry, Some(3)), "2 KB");
        entry.size = None;
        assert_eq!(format_entry_size(&entry, None), "\u{2014}");
    }

    #[test]
    fn breadcrumbs_start_at_home_or_root_and_keep_the_current_folder() {
        use fileflux::core::Location;
        use std::path::{Path, PathBuf};
        let dir = |p: &str| Location::Directory(PathBuf::from(p));
        let home = Path::new("/home/me");

        let shape = |crumbs: Vec<Crumb>| -> Vec<(String, CrumbKind, Location)> {
            crumbs
                .into_iter()
                .map(|crumb| (crumb.label, crumb.kind, crumb.target))
                .collect()
        };
        let item = |label: &str, kind, target: &str| (label.to_owned(), kind, dir(target));

        assert_eq!(
            shape(breadcrumbs(&dir("/home/me/Pictures/2026"), Some(home))),
            [
                item("Home", CrumbKind::Home, "/home/me"),
                item("Pictures", CrumbKind::Folder, "/home/me/Pictures"),
                item("2026", CrumbKind::Folder, "/home/me/Pictures/2026"),
            ]
        );
        assert_eq!(
            shape(breadcrumbs(&dir("/home/me"), Some(home))),
            [item("Home", CrumbKind::Home, "/home/me")],
            "home alone is one segment"
        );
        assert_eq!(
            shape(breadcrumbs(&dir("/var/log"), Some(home))),
            [
                item("/", CrumbKind::Root, "/"),
                item("var", CrumbKind::Folder, "/var"),
                item("log", CrumbKind::Folder, "/var/log"),
            ]
        );
        assert_eq!(
            shape(breadcrumbs(&dir("/"), None)),
            [item("/", CrumbKind::Root, "/")]
        );
        // /home/mentor is not under /home/me: component-wise prefix.
        assert_eq!(
            shape(breadcrumbs(&dir("/home/mentor"), Some(home))).first(),
            Some(&item("/", CrumbKind::Root, "/"))
        );
        assert_eq!(
            shape(breadcrumbs(&dir("/home/me/a/b/c/d/e/f"), Some(home))),
            [
                item("Home", CrumbKind::Home, "/home/me"),
                item("\u{2026}", CrumbKind::Ellipsis, "/home/me/a/b"),
                item("c", CrumbKind::Folder, "/home/me/a/b/c"),
                item("d", CrumbKind::Folder, "/home/me/a/b/c/d"),
                item("e", CrumbKind::Folder, "/home/me/a/b/c/d/e"),
                item("f", CrumbKind::Folder, "/home/me/a/b/c/d/e/f"),
            ],
            "leading folders collapse; the ellipsis goes to the nearest hidden parent"
        );
        assert_eq!(
            shape(breadcrumbs(&Location::Trash, Some(home))),
            [("Home Trash".to_owned(), CrumbKind::Folder, Location::Trash)]
        );
    }

    #[test]
    fn subbar_and_status_texts_count_and_size() {
        use fileflux::core::Entry;
        let make = |kind| Entry {
            name: "x".into(),
            path: "/x".into(),
            kind,
            is_symlink: false,
            size: None,
            modified: None,
            origin: None,
        };
        let mixed: Vec<Entry> = [EntryKind::Directory, EntryKind::Directory, EntryKind::File]
            .into_iter()
            .map(make)
            .collect();
        assert_eq!(folder_counts_label(&mixed), "2 folders \u{b7} 1 file");
        assert_eq!(folder_counts_label(&[make(EntryKind::File)]), "1 file");
        assert_eq!(folder_counts_label(&[]), "empty");

        assert_eq!(status_label(1, None, 0, 0), "1 item");
        assert_eq!(status_label(12, None, 0, 0), "12 items");
        assert_eq!(status_label(12, Some(3), 0, 0), "3 of 12 shown");
        assert_eq!(
            status_label(12, None, 3, 12_400_000),
            "3 selected \u{b7} 12.4 MB"
        );
    }

    #[test]
    fn sizes_use_the_one_decimal_si_formatter() {
        use fileflux::core::Entry;
        let mut file = Entry {
            name: "x".into(),
            path: "/x".into(),
            kind: EntryKind::File,
            is_symlink: false,
            size: Some(2_900_000),
            modified: None,
            origin: None,
        };
        assert_eq!(format_entry_size(&file, None), "2.9 MB");
        file.size = Some(999);
        assert_eq!(format_entry_size(&file, None), "999 B");
    }

    #[test]
    fn grid_rows_chunk_by_columns() {
        assert_eq!(grid_row(0, 4), 0);
        assert_eq!(grid_row(3, 4), 0);
        assert_eq!(grid_row(4, 4), 1);
        assert_eq!(grid_row(9, 4), 2);
        assert_eq!(grid_row(0, 0), 0, "no columns means one safe row");
    }
}
