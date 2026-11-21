use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fileflux::core::Location;
use fileflux::core::usage::{UsageEnd, UsageProgress, UsageTree, analyze};

use super::*;
use crate::explorer::arrange::Arrangement;
use crate::explorer::ops::{MenuCommand, MenuRow, MenuTarget, menu_rows};
use crate::explorer::state::View;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> io::Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "fileflux-usage-ui-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        Ok(Self(root))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
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

fn tree_of(root: &Path) -> io::Result<UsageTree> {
    match analyze(root, &AtomicBool::new(false), &UsageProgress::default())? {
        UsageEnd::Done(tree) => Ok(tree),
        UsageEnd::Cancelled => Err(io::Error::other("cancelled")),
    }
}

fn ready(root: &Path) -> io::Result<UsageSession> {
    let mut session = UsageSession::walking(root.to_path_buf());
    session.finish(tree_of(root)?);
    Ok(session)
}

fn names(rows: &[ShownRow]) -> Vec<String> {
    rows.iter().map(|row| row.name.clone()).collect()
}

// --- ranking -----------------------------------------------------------------

#[test]
fn rows_are_ranked_by_size_with_a_bar_share_icon_kind_and_counts() -> io::Result<()> {
    let scratch = Scratch::new("rank")?;
    sample(scratch.path())?;
    let session = ready(scratch.path())?;
    let rows = session.rows();
    assert_eq!(names(&rows), ["a", "b", "top.bin"]);
    assert!(rows.windows(2).all(|pair| pair[0].bytes >= pair[1].bytes));
    let total = session.summary().map(|summary| summary.bytes).unwrap_or(0);
    for row in &rows {
        assert!((f64::from(row.share) - row.bytes as f64 / total as f64).abs() < 1e-6);
        assert_eq!(row.depth, 0);
    }
    assert_eq!(rows[0].kind, ShownKind::Folder);
    assert_eq!(rows[0].items, 3, "one.bin, deep and deep/two.bin");
    assert!(rows[0].expandable && !rows[0].expanded);
    assert_eq!(rows[2].kind, ShownKind::File);
    assert!(!rows[2].expandable);
    assert_eq!(rows[0].path, scratch.path().join("a"));
    Ok(())
}

#[test]
fn an_expanded_folder_lists_its_children_indented_under_it() -> io::Result<()> {
    let scratch = Scratch::new("expand")?;
    sample(scratch.path())?;
    let mut session = ready(scratch.path())?;
    session.toggle(&scratch.path().join("a"));
    let rows = session.rows();
    assert_eq!(names(&rows), ["a", "one.bin", "deep", "b", "top.bin"]);
    assert_eq!(
        rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
        [0, 1, 1, 0, 0]
    );
    assert!(rows[0].expanded);
    session.toggle(&scratch.path().join("a"));
    assert_eq!(names(&session.rows()), ["a", "b", "top.bin"]);
    Ok(())
}

#[test]
fn rows_marks_an_unreadable_folder_incomplete() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = Scratch::new("incomplete")?;
    let root = scratch.path();
    fs::create_dir_all(root.join("locked"))?;
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000))?;
    let denied = fs::read_dir(root.join("locked")).is_err();
    let session = ready(root);
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755))?;
    if !denied {
        return Ok(()); // running as root
    }
    let rows = session?.rows();
    assert!(
        rows.iter()
            .any(|row| row.name == "locked" && row.incomplete)
    );
    Ok(())
}

#[test]
fn the_streamed_partial_totals_are_ranked_and_not_yet_actionable() {
    use fileflux::core::usage::{Partial, PartialRow};
    let mut session = UsageSession::walking(PathBuf::from("/data"));
    session.progress(Partial {
        rows: vec![
            PartialRow {
                name: "small".into(),
                is_folder: true,
                bytes: 10,
                items: 1,
                incomplete: false,
                done: true,
            },
            PartialRow {
                name: "big".into(),
                is_folder: true,
                bytes: 90,
                items: 9,
                incomplete: false,
                done: false,
            },
        ],
        bytes: 100,
        items: 10,
        ..Partial::default()
    });
    let rows = session.rows();
    assert_eq!(names(&rows), ["big", "small"]);
    assert!((rows[0].share - 0.9).abs() < 1e-6);
    assert!(rows[0].pending && !rows[1].pending);
    assert_eq!(rows[0].path, PathBuf::from("/data/big"));
    assert_eq!(session.activation(Path::new("/data/big")), Activation::None);
    assert!(session.status_text().starts_with("Measuring"));
}

// --- status line ---------------------------------------------------------------

#[test]
fn the_status_line_gives_the_total_and_the_item_count() -> io::Result<()> {
    let scratch = Scratch::new("status")?;
    sample(scratch.path())?;
    let session = ready(scratch.path())?;
    let summary = session
        .summary()
        .ok_or_else(|| io::Error::other("no summary"))?;
    assert_eq!(summary.items, 7);
    assert_eq!(
        session.status_text(),
        format!(
            "{} in 7 items",
            fileflux::core::places::format_bytes(summary.bytes)
        )
    );
    Ok(())
}

#[test]
fn a_stopped_or_failed_analysis_says_so() {
    let mut stopped = UsageSession::walking(PathBuf::from("/data"));
    stopped.stop();
    assert!(stopped.status_text().starts_with("Stopped"));
    let mut failed = UsageSession::walking(PathBuf::from("/data"));
    failed.fail("Permission denied".to_owned());
    assert_eq!(failed.status_text(), "Could not analyze: Permission denied");
    assert!(failed.rows().is_empty());
}

// --- drill down and cache ---------------------------------------------------------

#[test]
fn a_folder_of_the_finished_analysis_is_reused_not_walked_again() -> io::Result<()> {
    let scratch = Scratch::new("plan")?;
    sample(scratch.path())?;
    let session = ready(scratch.path())?;
    let inside = scratch.path().join("a/deep");
    assert_eq!(plan(Some(&session), &inside), Plan::Reuse);
    assert_eq!(plan(Some(&session), scratch.path()), Plan::Reuse);
    assert_eq!(plan(Some(&session), Path::new("/elsewhere")), Plan::Walk);
    assert_eq!(
        plan(Some(&session), &scratch.path().join("missing")),
        Plan::Walk
    );
    assert_eq!(plan(None, scratch.path()), Plan::Walk);
    Ok(())
}

#[test]
fn a_running_walk_is_kept_for_its_own_root_and_replaced_for_another() {
    let session = UsageSession::walking(PathBuf::from("/data"));
    assert_eq!(plan(Some(&session), Path::new("/data")), Plan::Keep);
    assert_eq!(plan(Some(&session), Path::new("/data/sub")), Plan::Walk);
}

#[test]
fn showing_a_cached_folder_ranks_its_children_and_clears_the_selection() -> io::Result<()> {
    let scratch = Scratch::new("show")?;
    sample(scratch.path())?;
    let mut session = ready(scratch.path())?;
    session.select(&scratch.path().join("top.bin"));
    session.toggle(&scratch.path().join("a"));
    session.show(&scratch.path().join("a"));
    assert_eq!(session.shown, scratch.path().join("a"));
    assert_eq!(session.selected, None);
    assert!(session.expanded.is_empty());
    assert_eq!(names(&session.rows()), ["one.bin", "deep"]);
    let summary = session
        .summary()
        .ok_or_else(|| io::Error::other("no summary"))?;
    assert_eq!(summary.items, 3);
    Ok(())
}

#[test]
fn activating_a_folder_drills_and_a_file_opens() -> io::Result<()> {
    let scratch = Scratch::new("activate")?;
    sample(scratch.path())?;
    let session = ready(scratch.path())?;
    assert_eq!(
        session.activation(&scratch.path().join("a")),
        Activation::Drill(scratch.path().join("a"))
    );
    assert_eq!(
        session.activation(&scratch.path().join("top.bin")),
        Activation::Open(scratch.path().join("top.bin"))
    );
    assert_eq!(
        session.activation(&scratch.path().join("a/one.bin")),
        Activation::Open(scratch.path().join("a/one.bin"))
    );
    Ok(())
}

#[test]
fn up_inside_the_analysis_goes_to_the_parent_usage_and_at_its_root_leaves_it() -> io::Result<()> {
    let scratch = Scratch::new("up")?;
    sample(scratch.path())?;
    let mut session = ready(scratch.path())?;
    assert_eq!(
        session.up_target(),
        None,
        "the root's Up is the plain parent folder"
    );
    session.show(&scratch.path().join("a/deep"));
    assert_eq!(session.up_target(), Some(scratch.path().join("a")));
    Ok(())
}

// --- cleanup ------------------------------------------------------------------------

#[test]
fn removing_items_subtracts_them_and_labels_the_totals_an_estimate() -> io::Result<()> {
    let scratch = Scratch::new("removed")?;
    sample(scratch.path())?;
    let mut session = ready(scratch.path())?;
    let before = session
        .summary()
        .ok_or_else(|| io::Error::other("no summary"))?;
    let a_bytes = session.rows()[0].bytes;
    session.select(&scratch.path().join("a"));
    assert!(!session.is_estimate());

    let subtracted = session.removed(&[scratch.path().join("a")]);
    assert_eq!(subtracted, 1);
    let after = session
        .summary()
        .ok_or_else(|| io::Error::other("no summary"))?;
    assert_eq!(after.bytes, before.bytes - a_bytes);
    assert_eq!(after.items, before.items - 4);
    assert_eq!(names(&session.rows()), ["b", "top.bin"]);
    assert_eq!(
        session.selected, None,
        "the removed item is no longer selected"
    );
    assert!(session.is_estimate());
    assert_eq!(
        session.status_text(),
        format!(
            "{} in {} items (estimate after cleanup)",
            fileflux::core::places::format_bytes(after.bytes),
            after.items
        )
    );
    Ok(())
}

#[test]
fn removing_what_the_analysis_does_not_hold_keeps_the_numbers_exact() -> io::Result<()> {
    let scratch = Scratch::new("removed-none")?;
    sample(scratch.path())?;
    let mut session = ready(scratch.path())?;
    let before = session.summary();
    assert_eq!(session.removed(&[PathBuf::from("/elsewhere/x")]), 0);
    assert_eq!(session.summary(), before);
    assert!(!session.is_estimate());
    Ok(())
}

#[test]
fn a_change_that_is_not_a_removal_marks_the_analysis_stale() -> io::Result<()> {
    let scratch = Scratch::new("stale")?;
    sample(scratch.path())?;
    let mut session = ready(scratch.path())?;
    assert!(!session.stale);
    session.mark_stale();
    assert!(session.stale);
    assert!(session.status_text().contains("changed since"));
    Ok(())
}

// --- menus --------------------------------------------------------------------------

fn commands(rows: &[MenuRow]) -> Vec<MenuCommand> {
    rows.iter()
        .filter_map(|row| match row {
            MenuRow::Item(spec) => Some(spec.command),
            _ => None,
        })
        .collect()
}

#[test]
fn a_row_menu_offers_open_containing_folder_copy_path_trash_and_delete() {
    {
        let rows = usage_menu_rows();
        assert_eq!(
            commands(&rows),
            [
                MenuCommand::Open,
                MenuCommand::OpenContainingFolder,
                MenuCommand::CopyPath,
                MenuCommand::MoveToTrash,
                MenuCommand::DeletePermanently,
            ]
        );
        let danger: Vec<MenuCommand> = rows
            .iter()
            .filter_map(|row| match row {
                MenuRow::Item(spec) if spec.danger => Some(spec.command),
                _ => None,
            })
            .collect();
        assert_eq!(
            danger,
            [MenuCommand::MoveToTrash, MenuCommand::DeletePermanently]
        );
    }
}

fn menu_target(selected: usize, folder_selected: bool) -> MenuTarget {
    MenuTarget {
        in_trash: false,
        selected,
        folder_is_empty: false,
        terminal: false,
        can_paste: false,
        folder_selected,
        only_files: selected > 0 && !folder_selected,
        folder_favorite: false,
    }
}

#[test]
fn the_empty_space_menu_of_a_folder_offers_analyze_disk_usage() {
    let mut rows = menu_rows(&menu_target(0, false), &Arrangement::default());
    add_analyze_row(&mut rows, analyze_offer(true, 0, false));
    assert!(commands(&rows).contains(&MenuCommand::AnalyzeDiskUsage));
    let label = rows.iter().find_map(|row| match row {
        MenuRow::Item(spec) if spec.command == MenuCommand::AnalyzeDiskUsage => Some(spec.label),
        _ => None,
    });
    assert_eq!(label, Some("Analyze Disk Usage"));
}

#[test]
fn a_single_selected_folder_offers_it_and_nothing_else_does() {
    let mut folder = menu_rows(&menu_target(1, true), &Arrangement::default());
    add_analyze_row(&mut folder, analyze_offer(true, 1, true));
    assert!(commands(&folder).contains(&MenuCommand::AnalyzeDiskUsage));

    let cases = [
        (true, 1, false, "a single file"),
        (true, 2, true, "several entries"),
        (
            false,
            0,
            false,
            "Trash, Recent, search results or another usage view",
        ),
        (false, 1, true, "a folder outside a real folder"),
    ];
    for (in_folder, selected, folder_selected, what) in cases {
        let mut rows = menu_rows(
            &menu_target(selected, folder_selected),
            &Arrangement::default(),
        );
        add_analyze_row(
            &mut rows,
            analyze_offer(in_folder, selected, folder_selected),
        );
        assert!(
            !commands(&rows).contains(&MenuCommand::AnalyzeDiskUsage),
            "{what}"
        );
    }
}

// --- the location ---------------------------------------------------------------------

fn usage(root: &str) -> Location {
    Location::Usage {
        root: PathBuf::from(root),
    }
}

#[test]
fn a_usage_location_is_refused_inside_the_trash() {
    use crate::explorer::ops::refuses_entering;
    let trash = Path::new("/home/u/.local/share/Trash");
    assert!(refuses_entering(
        &usage("/home/u/.local/share/Trash/files/x"),
        Some(trash)
    ));
    assert!(!refuses_entering(&usage("/home/u/data"), Some(trash)));
    assert!(!refuses_entering(&usage("/home/u/data"), None));
}

#[test]
fn a_usage_tab_is_saved_as_the_folder_it_measures() {
    let tab =
        crate::explorer::persist::session_tab(&usage("/a/b"), View::List, &Arrangement::default());
    assert_eq!(tab.path.as_deref(), Some(Path::new("/a/b")));
    assert!(!tab.trash && !tab.recent);
}

#[test]
fn a_usage_location_takes_nothing_pasted_created_or_linked_and_is_not_watched() {
    let location = usage("/a");
    assert!(location.lists_files_from_elsewhere());
    assert!(crate::explorer::links::refusal(&location).is_some());
    assert!(crate::explorer::templates::refusal(&location).is_some());
    assert!(
        !crate::explorer::tabs::Affected::Folders(vec![Location::Directory(PathBuf::from("/a"))])
            .touches(&location)
    );
    assert!(crate::explorer::tabs::Affected::Everything.touches(&location));
}

#[test]
fn a_usage_tab_is_titled_and_has_breadcrumbs_that_end_in_the_analysis() {
    use crate::explorer::util::breadcrumbs;
    let location = usage("/srv/data");
    assert_eq!(
        crate::explorer::tabs::tab_label(&location, None),
        "Usage: data"
    );
    let crumbs = breadcrumbs(&location, None);
    let last = crumbs
        .last()
        .map(|crumb| (crumb.label.as_str(), crumb.target.clone()));
    assert_eq!(last, Some(("Disk usage", location.clone())));
    assert!(
        crumbs
            .iter()
            .any(|crumb| crumb.target == Location::Directory(PathBuf::from("/srv/data")))
    );
}

#[test]
fn up_from_a_usage_location_goes_to_the_measured_folder_and_back_returns() {
    use fileflux::core::navigation::Navigation;
    let mut nav = Navigation::new(Location::Directory(PathBuf::from("/srv/data")));
    nav.visit(usage("/srv/data"));
    assert!(nav.can_up());
    assert!(nav.up());
    assert_eq!(
        nav.current(),
        &Location::Directory(PathBuf::from("/srv/data"))
    );
    assert!(nav.back());
    assert_eq!(nav.current(), &usage("/srv/data"));
}

#[test]
fn only_distinct_paths_stay_in_the_expanded_set() -> io::Result<()> {
    let scratch = Scratch::new("set")?;
    sample(scratch.path())?;
    let mut session = ready(scratch.path())?;
    session.toggle(&scratch.path().join("a"));
    session.toggle(&scratch.path().join("b"));
    let expected: BTreeSet<PathBuf> = [scratch.path().join("a"), scratch.path().join("b")].into();
    assert_eq!(session.expanded, expected);
    Ok(())
}

#[test]
fn only_a_change_under_the_analysis_root_can_make_it_stale() {
    use crate::explorer::tabs::Affected;
    let root = Path::new("/srv/data");
    let folder = |path: &str| Affected::Folders(vec![Location::Directory(PathBuf::from(path))]);
    assert!(touches_root(&Affected::Everything, root));
    assert!(touches_root(&folder("/srv/data/a"), root));
    assert!(
        touches_root(&folder("/srv"), root),
        "a change above the root may reach it"
    );
    assert!(!touches_root(&folder("/home/u"), root));
    assert!(!touches_root(
        &Affected::Folders(vec![Location::Trash]),
        root
    ));
}
