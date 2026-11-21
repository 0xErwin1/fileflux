use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-explorer-archive-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir)?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn p(text: &str) -> PathBuf {
    PathBuf::from(text)
}

fn selected(items: &[(&str, bool)]) -> Vec<(PathBuf, bool)> {
    items.iter().map(|(name, dir)| (p(name), *dir)).collect()
}

fn commands(rows: &[MenuRow]) -> Vec<MenuCommand> {
    rows.iter()
        .filter_map(|row| match row {
            MenuRow::Item(spec) => Some(spec.command),
            _ => None,
        })
        .collect()
}

fn menu() -> Vec<MenuRow> {
    vec![
        MenuRow::Item(MenuItemSpec::new(MenuCommand::Open, "Open")),
        MenuRow::Separator,
        MenuRow::Item(MenuItemSpec::new(MenuCommand::Paste, "Paste")),
        MenuRow::Separator,
        MenuRow::Item(MenuItemSpec::new(MenuCommand::CopyPath, "Copy path")),
    ]
}

#[test]
fn compress_is_offered_for_any_selection_in_a_folder() {
    let any = selected(&[("/d/a", true), ("/d/b.txt", false)]);
    assert_eq!(
        offer_for(true, &any, true),
        ArchiveOffer {
            compress: true,
            extract: false,
            tool: true
        }
    );
    assert!(!offer_for(false, &any, true).compress);
    assert!(!offer_for(true, &[], true).compress);
}

#[test]
fn extract_is_offered_for_one_archive_file_only() {
    let one = selected(&[("/d/a.tar.gz", false)]);
    assert!(offer_for(true, &one, true).extract);
    assert!(!offer_for(false, &one, true).extract);
    assert!(!offer_for(true, &selected(&[("/d/a.txt", false)]), true).extract);
    assert!(!offer_for(true, &selected(&[("/d/a.zip", true)]), true).extract);
    assert!(
        !offer_for(
            true,
            &selected(&[("/d/a.zip", false), ("/d/b.zip", false)]),
            true
        )
        .extract
    );
}

#[test]
fn rows_go_after_the_paste_group() {
    let mut rows = menu();
    add_rows(
        &mut rows,
        ArchiveOffer {
            compress: true,
            extract: true,
            tool: true,
        },
    );
    assert_eq!(
        commands(&rows),
        [
            MenuCommand::Open,
            MenuCommand::Paste,
            MenuCommand::ExtractHere,
            MenuCommand::ExtractTo,
            MenuCommand::Compress,
            MenuCommand::CopyPath,
        ]
    );
    // Paste's own separator, the three rows, one separator, Copy path.
    assert!(matches!(rows.get(3), Some(MenuRow::Separator)));
    assert!(matches!(rows.get(7), Some(MenuRow::Separator)));
    assert_eq!(rows.len(), 9);
}

#[test]
fn without_bsdtar_the_rows_are_disabled_and_say_why() -> Result<(), Box<dyn std::error::Error>> {
    let mut rows = menu();
    add_rows(
        &mut rows,
        ArchiveOffer {
            compress: true,
            extract: false,
            tool: false,
        },
    );
    let compress = rows.iter().find_map(|row| match row {
        MenuRow::Item(spec) if spec.command == MenuCommand::Compress => Some(spec),
        _ => None,
    });
    let compress = compress.ok_or("the row is shown")?;
    assert!(!compress.enabled);
    assert_eq!(compress.label, "Compress\u{2026} (needs bsdtar)");
    assert_eq!(
        MISSING_TOOL,
        "Install libarchive (bsdtar) to compress and extract"
    );
    Ok(())
}

#[test]
fn nothing_is_added_when_nothing_is_offered() {
    let mut rows = menu();
    add_rows(
        &mut rows,
        ArchiveOffer {
            compress: false,
            extract: false,
            tool: true,
        },
    );
    assert_eq!(rows, menu());
}

#[test]
fn destinations_resolve_home_and_relative_paths() {
    let home = p("/home/me");
    let here = p("/work/here");
    assert_eq!(resolve_destination("~", Some(&home), &here), home);
    assert_eq!(
        resolve_destination(" ~/x ", Some(&home), &here),
        p("/home/me/x")
    );
    assert_eq!(
        resolve_destination("/abs/dir", Some(&home), &here),
        p("/abs/dir")
    );
    assert_eq!(
        resolve_destination("sub", Some(&home), &here),
        p("/work/here/sub")
    );
    assert_eq!(resolve_destination("~", None, &here), here);
}

fn tool() -> Option<PathBuf> {
    let tool = fileflux::core::archive::tool().map(Path::to_path_buf);
    if tool.is_none() {
        eprintln!("skipped: bsdtar is not on PATH");
    }
    tool
}

#[test]
fn compress_report_selects_the_archive_and_can_be_undone() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(tool) = tool() else {
        return Ok(());
    };
    let scratch = Scratch::new()?;
    fs::write(scratch.0.join("a.txt"), b"a")?;
    let sink = Sink::new(Unit::Items);
    let report = run_compress(
        &tool,
        &scratch.0,
        &[OsString::from("a.txt")],
        OsStr::new("a.txt"),
        Format::TarGz,
        &sink,
    );
    let made = scratch.0.join("a.txt.tar.gz");
    assert!(made.is_file());
    assert_eq!(report.select, std::slice::from_ref(&made));
    assert!(matches!(report.undo, Some(UndoAction::Archive { ref path, .. }) if *path == made));
    let notice = report.notice.ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Success);
    assert_eq!(
        notice.text,
        "Created \u{201c}a.txt.tar.gz\u{201d} from 1 item"
    );
    Ok(())
}

#[test]
fn cancelled_jobs_leave_nothing_and_say_so() -> Result<(), Box<dyn std::error::Error>> {
    let Some(tool) = tool() else {
        return Ok(());
    };
    let scratch = Scratch::new()?;
    fs::write(scratch.0.join("a.txt"), b"a")?;
    let sink = Sink::new(Unit::Items);
    sink.request_cancel();
    let report = run_compress(
        &tool,
        &scratch.0,
        &[OsString::from("a.txt")],
        OsStr::new("x"),
        Format::Zip,
        &sink,
    );
    assert!(report.cancelled);
    assert!(report.undo.is_none());
    assert_eq!(
        report.notice.map(|n| n.text).as_deref(),
        Some("Compressing cancelled. Nothing was created.")
    );
    let names: Vec<_> = fs::read_dir(&scratch.0)?.collect::<Result<_, _>>()?;
    assert_eq!(names.len(), 1);
    Ok(())
}

#[test]
fn an_unsafe_archive_is_a_clear_error_and_writes_nothing() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(tool) = tool() else {
        return Ok(());
    };
    let scratch = Scratch::new()?;
    // A real archive whose member is renamed to a `..` path by bsdtar's -s.
    fs::write(scratch.0.join("f.txt"), b"x")?;
    let evil = scratch.0.join("evil.tar");
    let made = std::process::Command::new(&tool)
        .arg("-cf")
        .arg(&evil)
        .arg("-C")
        .arg(&scratch.0)
        .arg("-s")
        .arg("|f.txt|../outside.txt|")
        .arg("f.txt")
        .output()?;
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let dest = scratch.0.join("dest");
    fs::create_dir(&dest)?;
    let sink = Sink::new(Unit::Items);
    let report = run_extract(&tool, &evil, &dest, &sink);
    let notice = report.notice.ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Error);
    assert_eq!(
        notice.text,
        "Could not extract \u{201c}evil.tar\u{201d}: The archive is unsafe: \u{201c}../outside.txt\u{201d} points outside the destination. Nothing was extracted."
    );
    assert!(report.undo.is_none());
    assert!(report.select.is_empty());
    assert_eq!(fs::read_dir(&dest)?.count(), 0);
    assert!(!scratch.0.join("outside.txt").exists());
    Ok(())
}

#[test]
fn extract_report_names_the_new_folder_and_selects_it() -> Result<(), Box<dyn std::error::Error>> {
    let Some(tool) = tool() else {
        return Ok(());
    };
    let scratch = Scratch::new()?;
    fs::write(scratch.0.join("a.txt"), b"a")?;
    let never = std::sync::atomic::AtomicBool::new(false);
    let archive = fileflux::core::archive::compress(
        &tool,
        &scratch.0,
        &[OsString::from("a.txt")],
        OsStr::new("pack"),
        Format::Zip,
        &never,
    )?;
    let dest = scratch.0.join("out");
    fs::create_dir(&dest)?;
    let sink = Sink::new(Unit::Items);
    let report = run_extract(&tool, &archive, &dest, &sink);
    assert_eq!(report.select, [dest.join("pack")]);
    assert!(matches!(report.undo, Some(UndoAction::Archive { .. })));
    let notice = report.notice.ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Success);
    assert_eq!(
        notice.text,
        "Extracted \u{201c}pack.zip\u{201d} to \u{201c}pack\u{201d} (1 item)"
    );
    assert_eq!(fs::read(dest.join("pack/a.txt"))?, b"a");
    Ok(())
}
