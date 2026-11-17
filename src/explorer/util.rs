//! Display helpers for the explorer: honest metadata formatting. Sizes and
//! times come straight from the scan; nothing is invented for missing
//! metadata.

use std::time::{SystemTime, UNIX_EPOCH};

use fileflux::core::EntryKind;

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

/// The UTC timestamp as `YYYY-MM-DD HH:MM`. Fileflux shows UTC explicitly
/// instead of reinventing a timezone database; `None` means the scan could
/// not read the modification time.
pub(crate) fn format_modified(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "\u{2014}".to_owned();
    };
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    format!("{} UTC", format_civil(seconds))
}

/// Seconds since the Unix epoch as `YYYY-MM-DD HH:MM` in UTC (Howard
/// Hinnant's civil-from-days algorithm).
fn format_civil(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let time_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3_600;
    let minute = (time_of_day % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 into
/// (year, month, day) in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    // January and February belong to the following Gregorian year.
    (year + i64::from(month <= 2), month, day)
}

/// The type column label: honest about what the scan could resolve.
/// A symlink that could not be resolved is a broken link; a resolved symlink
/// shows the target's kind.
pub(crate) fn format_kind(entry: &fileflux::core::Entry) -> String {
    if entry.is_symlink && entry.kind == EntryKind::Symlink {
        return "Broken link".to_owned();
    }
    match entry.kind {
        EntryKind::Directory => "Folder".to_owned(),
        EntryKind::File => "File".to_owned(),
        EntryKind::Symlink => "Broken link".to_owned(),
        EntryKind::Other => "Special".to_owned(),
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
    use std::time::Duration;

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

    #[test]
    fn timestamps_render_as_utc_civil_dates() -> TestResult {
        let epoch = UNIX_EPOCH;
        assert_eq!(format_modified(Some(epoch)), "1970-01-01 00:00 UTC");
        // 2026-01-30 14:22:00 UTC.
        let time = epoch
            .checked_add(Duration::from_secs(1_769_782_920))
            .ok_or("test timestamp overflows")?;
        assert_eq!(format_modified(Some(time)), "2026-01-30 14:22 UTC");
        assert_eq!(
            format_modified(None),
            "\u{2014}",
            "missing metadata is honest"
        );
        Ok(())
    }

    #[test]
    fn kind_labels_distinguish_broken_links_from_resolved_targets() {
        use fileflux::core::Entry;
        use std::ffi::OsString;
        let mut file = Entry {
            name: OsString::from("a.txt"),
            path: "/a.txt".into(),
            kind: EntryKind::File,
            is_symlink: false,
            size: None,
            modified: None,
            origin: None,
        };
        assert_eq!(format_kind(&file), "File");
        file.is_symlink = true;
        assert_eq!(
            format_kind(&file),
            "File",
            "a resolved symlink shows its target kind"
        );
        file.kind = EntryKind::Symlink;
        assert_eq!(format_kind(&file), "Broken link");
        file.kind = EntryKind::Directory;
        assert_eq!(
            format_kind(&file),
            "Folder",
            "a symlinked folder shows Folder"
        );
        file.is_symlink = false;
        assert_eq!(format_kind(&file), "Folder");
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
