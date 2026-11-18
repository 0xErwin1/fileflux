//! Display helpers for the explorer: honest metadata formatting. Sizes and
//! times come straight from the scan; nothing is invented for missing
//! metadata.

use std::time::SystemTime;

use chrono::{DateTime, Datelike as _, TimeZone, Utc};

/// Human-readable byte size; `None` renders as an em dash (directories and
/// entries whose metadata could not be read).
pub(crate) fn format_size(size: Option<u64>) -> String {
    match size {
        None => "\u{2014}".to_owned(),
        Some(0) => "0 B".to_owned(),
        Some(bytes) => {
            const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
            let mut value = bytes as f64;
            let mut unit = 0;
            while value >= 1024.0 && unit + 1 < UNITS.len() {
                value /= 1024.0;
                unit += 1;
            }
            let unit_name = UNITS.get(unit).unwrap_or(&UNITS[0]);
            if unit == 0 {
                format!("{bytes} B")
            } else if value >= 100.0 {
                format!("{value:.0} {unit_name}")
            } else {
                format!("{value:.1} {unit_name}")
            }
        }
    }
}

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
        format_size(entry.size)
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

    #[test]
    fn sizes_read_in_b_and_scaled_units() {
        assert_eq!(format_size(None), "\u{2014}", "directories show no size");
        assert_eq!(format_size(Some(0)), "0 B");
        assert_eq!(format_size(Some(7)), "7 B");
        assert_eq!(format_size(Some(1_023)), "1023 B");
        assert_eq!(format_size(Some(1_024)), "1.0 KB");
        assert_eq!(format_size(Some(48_234_496)), "46.0 MB");
        assert_eq!(format_size(Some(2_147_483_648 * 2)), "4.0 GB");
        assert_eq!(format_size(Some(2_199_023_255_552)), "2.0 TB");
        assert_eq!(format_size(Some(2_251_799_813_685_248)), "2048 TB");
    }

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
        assert_eq!(format_entry_size(&entry, Some(3)), "2.0 KB");
        entry.size = None;
        assert_eq!(format_entry_size(&entry, None), "\u{2014}");
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
