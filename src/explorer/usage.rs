//! The disk usage view of a tab: the analysis session (what a walk of
//! [`fileflux::core::usage`] found and how the view stands on it), the
//! decisions that do not need a window (ranking, drill-down reuse, the
//! estimate after a cleanup, the menus) and the explorer methods that drive
//! the walk and the interactions. Drawing is in [`super::usage_view`].
//!
//! A tab on [`Location::Usage`] holds one [`UsageSession`]. Drilling into a
//! folder of a finished analysis only changes which folder is shown (the walk
//! measured every folder below the root), so it is instant; a folder outside
//! the analysis, or an analysis that is still walking, starts a new walk.
//! Leaving the view drops the session, which cancels the walk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fileflux::core::Location;
use fileflux::core::places::{format_bytes, gvfs_root_from_env, volume_usage};
use fileflux::core::usage::{
    Item, Partial, PartialRow, Row, UsageEnd, UsageProgress, UsageTree, analyze,
};
use fileflux::core::usage_chart::{self, Move, Segment, Target};
use gpui::{Context, Pixels, Point, ScrollStrategy, Task, UniformListScrollHandle, Window};

use super::actions::MenuOn;
use super::inspector::du_auto_allowed;
use super::menu_tree::Shown;
use super::ops::{MenuCommand, MenuItemSpec, MenuRow};
use super::state::Transition;
use super::tabs::Affected;
use super::view::{Explorer, TabId};

/// How often the UI collects the running totals of a walk.
const USAGE_TICK: Duration = Duration::from_millis(150);

/// The most rows the list builds for one folder (expanded folders included).
pub(crate) const MAX_LIST_ROWS: usize = 5_000;

/// The label of the entry-point row.
pub(crate) const ANALYZE_LABEL: &str = "Analyze Disk Usage";

/// What the session holds of the walk.
pub(crate) enum Phase {
    /// Not started: the folder is on a network or phone mount, where a walk
    /// is slow and may wake the device, so the user is asked first (the
    /// same decision as the details panel's size, see
    /// [`needs_confirmation`]).
    Ask,
    /// Running: the per-child totals counted so far.
    Walking(Partial),
    /// Stopped by the user: what was counted before it stopped.
    Stopped(Partial),
    /// Finished.
    Ready(UsageTree),
    Failed(String),
}

/// The pointer over a chart segment.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChartHover {
    /// Index into [`UsageSession::segments`].
    pub(crate) segment: usize,
    /// The pointer, relative to the chart's top left corner.
    pub(crate) at: Point<Pixels>,
}

/// What the volume holding the analysis root reported as free (`statvfs`,
/// the sidebar meter's source), at the end of the analysis and after the
/// last cleanup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VolumeFree {
    pub(crate) baseline: Option<u64>,
    pub(crate) now: Option<u64>,
}

/// A reading of the volume's free space, from the background executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreeReading {
    /// At the end of the analysis.
    Before(Option<u64>),
    /// After a cleanup.
    Now(Option<u64>),
}

/// One analysis and the view's state on it.
pub(crate) struct UsageSession {
    /// The folder the walk measured.
    pub(crate) analysis_root: PathBuf,
    /// The folder whose children are ranked: the root, or a folder of the
    /// finished analysis the user drilled into.
    pub(crate) shown: PathBuf,
    pub(crate) phase: Phase,
    /// The selected item, shared by the chart and the list.
    pub(crate) selected: Option<PathBuf>,
    /// The folders the list shows opened.
    pub(crate) expanded: BTreeSet<PathBuf>,
    /// Something changed under the root that the numbers do not reflect and
    /// that was not a removal (an undo, a paste): analyze again to measure.
    pub(crate) stale: bool,
    pub(crate) hover: Option<ChartHover>,
    /// The chart's segments for the shown folder (empty until finished).
    pub(crate) segments: Arc<Vec<Segment>>,
    /// How many rings the chart draws (see [`usage_chart::zoom_rings`]).
    pub(crate) rings: u32,
    /// The slice the keyboard is on, an index into `segments`: drawn with a
    /// focus outline.
    pub(crate) cursor: Option<usize>,
    /// The folder whose folded "Other" slice is selected, instead of an item.
    pub(crate) other: Option<fileflux::core::usage::NodeId>,
    /// The list rows the selected "Other" slice stands for, as a path and
    /// whether the row is a folder's bucket of other files.
    pub(crate) marked: BTreeSet<(PathBuf, bool)>,
    pub(crate) free: VolumeFree,
    /// Which free-space reading is the awaited one; an older reading is
    /// dropped when it arrives.
    free_ticket: u64,
    pub(crate) list_scroll: UniformListScrollHandle,
}

/// What a row is, for its icon and what activating it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShownKind {
    Folder,
    File,
    /// The files of a folder that are not listed one by one.
    Other,
}

/// One row of the list, as drawn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShownRow {
    pub(crate) path: PathBuf,
    /// The name, lossy.
    pub(crate) name: String,
    pub(crate) kind: ShownKind,
    pub(crate) bytes: u64,
    /// Entries below a folder, files in the bucket, 0 for a file.
    pub(crate) items: u64,
    pub(crate) incomplete: bool,
    /// Share of the shown folder's total, for the bar (0 to 1).
    pub(crate) share: f32,
    /// How deep under the shown folder (expanded children are indented).
    pub(crate) depth: usize,
    pub(crate) expandable: bool,
    pub(crate) expanded: bool,
    /// The walk has not finished this row: its numbers are still growing.
    pub(crate) pending: bool,
}

/// The totals of the shown folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) bytes: u64,
    pub(crate) items: u64,
    pub(crate) incomplete: bool,
}

/// What activating a row (double click, Enter) does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Activation {
    /// Show the usage of this folder.
    Drill(PathBuf),
    /// Open this file as usual.
    Open(PathBuf),
    None,
}

/// What reaching a [`Location::Usage`] does to the tab's session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// The walk for this root is already running.
    Keep,
    /// The folder is inside the finished analysis: only show it.
    Reuse,
    /// Measure it.
    Walk,
}

/// Decides what a tab does on reaching `Usage { root }` with its `session`.
pub(crate) fn plan(session: Option<&UsageSession>, root: &Path) -> Plan {
    let Some(session) = session else {
        return Plan::Walk;
    };
    match &session.phase {
        Phase::Ready(tree) if tree.find(root).is_some() => Plan::Reuse,
        Phase::Walking(_) if session.analysis_root == root => Plan::Keep,
        _ => Plan::Walk,
    }
}

impl UsageSession {
    /// A session whose walk of `root` has just started.
    pub(crate) fn walking(root: PathBuf) -> Self {
        Self {
            shown: root.clone(),
            analysis_root: root,
            phase: Phase::Walking(Partial::default()),
            selected: None,
            expanded: BTreeSet::new(),
            stale: false,
            hover: None,
            segments: Arc::new(Vec::new()),
            rings: usage_chart::RING_DEPTH,
            cursor: None,
            other: None,
            marked: BTreeSet::new(),
            free: VolumeFree::default(),
            free_ticket: 0,
            list_scroll: UniformListScrollHandle::new(),
        }
    }

    /// A session that waits for the user's go-ahead before walking `root`.
    pub(crate) fn asking(root: PathBuf) -> Self {
        Self {
            phase: Phase::Ask,
            ..Self::walking(root)
        }
    }

    pub(crate) fn is_asking(&self) -> bool {
        matches!(self.phase, Phase::Ask)
    }

    /// Takes the next free-space reading's ticket: any reading asked for
    /// before it is out of date.
    pub(crate) fn next_free_ticket(&mut self) -> u64 {
        self.free_ticket += 1;
        self.free_ticket
    }

    /// Stores a reading made for `ticket`; `false` (and nothing stored) when
    /// a newer one was asked for since.
    pub(crate) fn accept_free(&mut self, ticket: u64, reading: FreeReading) -> bool {
        if ticket != self.free_ticket {
            return false;
        }
        match reading {
            FreeReading::Before(free) => self.set_free_before(free),
            FreeReading::Now(free) => self.set_free_now(free),
        }
        true
    }

    pub(crate) fn set_free_before(&mut self, free: Option<u64>) {
        self.free.baseline = free;
    }

    pub(crate) fn set_free_now(&mut self, free: Option<u64>) {
        self.free.now = free;
    }

    /// The streamed totals of the running walk.
    pub(crate) fn progress(&mut self, partial: Partial) {
        if matches!(self.phase, Phase::Walking(_)) {
            self.phase = Phase::Walking(partial);
        }
    }

    /// The walk finished: the tree replaces the streamed totals.
    pub(crate) fn finish(&mut self, tree: UsageTree) {
        self.phase = Phase::Ready(tree);
        self.refresh_chart();
    }

    /// The walk was stopped: the last streamed totals stay.
    pub(crate) fn stop(&mut self) {
        if let Phase::Walking(partial) = &self.phase {
            self.phase = Phase::Stopped(partial.clone());
        }
    }

    pub(crate) fn fail(&mut self, message: String) {
        self.phase = Phase::Failed(message);
    }

    pub(crate) fn is_walking(&self) -> bool {
        matches!(self.phase, Phase::Walking(_))
    }

    /// The finished analysis, when there is one.
    pub(crate) fn tree(&self) -> Option<&UsageTree> {
        match &self.phase {
            Phase::Ready(tree) => Some(tree),
            _ => None,
        }
    }

    /// The totals were adjusted by subtraction after a cleanup.
    pub(crate) fn is_estimate(&self) -> bool {
        self.tree().is_some_and(UsageTree::is_estimate)
    }

    /// Shows the usage of `folder` of the finished analysis.
    pub(crate) fn show(&mut self, folder: &Path) {
        self.shown = folder.to_path_buf();
        self.selected = None;
        self.drop_other();
        self.expanded.clear();
        self.hover = None;
        self.refresh_chart();
    }

    pub(crate) fn select(&mut self, path: &Path) {
        self.selected = Some(path.to_path_buf());
        self.cursor = None;
        self.drop_other();
    }

    fn drop_other(&mut self) {
        self.other = None;
        self.marked.clear();
    }

    /// Selects what the slice `index` stands for, shared with the list: an
    /// item is the selection; the "Other" slice marks the folded items in
    /// the list (opening their folder there). The keyboard cursor moves to
    /// it. `false` for a slice that stands for neither (a streamed row).
    pub(crate) fn select_segment(&mut self, index: usize) -> bool {
        let Some(target) = self.segments.get(index).map(|segment| segment.target) else {
            return false;
        };
        match target {
            Target::Item(item) => {
                let Some(tree) = self.tree() else {
                    return false;
                };
                let path = item_path(tree, item);
                self.select(&path);
            }
            Target::Other { dir, count, .. } => self.select_other(dir, count),
            Target::Row(_) => return false,
        }
        self.cursor = Some(index);
        true
    }

    fn select_other(&mut self, dir: fileflux::core::usage::NodeId, count: u64) {
        let Some(tree) = self.tree() else {
            return;
        };
        let folder = tree.path_of(dir);
        let marked: BTreeSet<(PathBuf, bool)> = usage_chart::folded_items(tree, dir, count)
            .into_iter()
            .map(|item| (item_path(tree, item), matches!(item, Item::OtherFiles(_))))
            .collect();
        // The folded items are rows of their folder: it and every folder
        // between it and the shown one open, so the rows are in the list.
        let opened: Vec<PathBuf> = folder
            .ancestors()
            .take_while(|path| *path != self.shown && path.starts_with(&self.shown))
            .map(Path::to_path_buf)
            .collect();
        self.selected = None;
        self.other = Some(dir);
        self.marked = marked;
        self.expanded.extend(opened);
    }

    /// Whether the row is one of the items the selected "Other" slice folds.
    pub(crate) fn is_marked(&self, row: &ShownRow) -> bool {
        !self.marked.is_empty()
            && self
                .marked
                .contains(&(row.path.clone(), row.kind == ShownKind::Other))
    }

    /// Where Enter goes on the selected "Other" slice: into its folder,
    /// unless that is the one shown.
    pub(crate) fn other_activation(&self) -> Option<PathBuf> {
        let tree = self.tree()?;
        let folder = tree.path_of(self.other?);
        (folder != self.shown).then_some(folder)
    }

    /// A key moved the chart cursor: selects the slice it reaches. `false`
    /// when there is nothing to move on or nowhere to go.
    pub(crate) fn move_cursor(&mut self, movement: Move) -> bool {
        if self.tree().is_none() {
            return false;
        }
        let from = self
            .cursor
            .filter(|index| *index < self.segments.len())
            .or_else(|| self.selected_segment());
        match usage_chart::step(&self.segments, from, movement) {
            Some(index) => self.select_segment(index),
            None => false,
        }
    }

    /// Scrolls the list to the first row of the selection (the selected item
    /// or the items the "Other" slice folds).
    pub(crate) fn reveal_selection(&self) {
        let rows = self.rows();
        let first = rows
            .iter()
            .position(|row| self.selected.as_ref() == Some(&row.path) || self.is_marked(row));
        if let Some(index) = first {
            self.list_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    /// Zooms the chart by `delta` rings; `false` when it stays as it is.
    pub(crate) fn zoom_rings(&mut self, delta: i32) -> bool {
        let rings = usage_chart::zoom_rings(self.rings, delta);
        if rings == self.rings {
            return false;
        }
        self.rings = rings;
        self.refresh_chart();
        true
    }

    /// Opens or closes a folder row of the list.
    pub(crate) fn toggle(&mut self, folder: &Path) {
        if !self.expanded.remove(folder) {
            self.expanded.insert(folder.to_path_buf());
        }
    }

    pub(crate) fn mark_stale(&mut self) {
        self.stale = true;
    }

    fn shown_id(&self) -> Option<fileflux::core::usage::NodeId> {
        let tree = self.tree()?;
        Some(tree.find(&self.shown).unwrap_or(UsageTree::ROOT))
    }

    /// Lays the chart out again for the shown folder.
    pub(crate) fn refresh_chart(&mut self) {
        self.hover = None;
        let segments = match (self.tree(), self.shown_id()) {
            (Some(tree), Some(id)) => usage_chart::layout(tree, id, self.rings),
            _ => Vec::new(),
        };
        self.segments = Arc::new(segments);
        // The cursor stays on what is selected, wherever it now is drawn.
        self.cursor = self.selected_segment();
    }

    /// The totals of the shown folder.
    pub(crate) fn summary(&self) -> Option<Summary> {
        match &self.phase {
            Phase::Ready(tree) => {
                let id = self.shown_id()?;
                Some(Summary {
                    bytes: tree.total(id),
                    items: tree.item_count(id),
                    incomplete: tree.is_incomplete(id),
                })
            }
            Phase::Walking(partial) | Phase::Stopped(partial) => Some(Summary {
                bytes: partial.bytes,
                items: partial.items,
                incomplete: partial.incomplete,
            }),
            Phase::Ask | Phase::Failed(_) => None,
        }
    }

    /// The status line: the total and the item count, and what qualifies it.
    pub(crate) fn status_text(&self) -> String {
        let counted = |summary: &Summary| {
            format!(
                "{} in {}",
                format_bytes(summary.bytes),
                items_label(summary.items)
            )
        };
        match (&self.phase, self.summary()) {
            (Phase::Failed(message), _) => format!("Could not analyze: {message}"),
            (Phase::Ask, _) => ASK_NOTICE.to_owned(),
            (Phase::Walking(_), Some(summary)) => {
                format!("Measuring\u{2026} {} so far", counted(&summary))
            }
            (Phase::Stopped(_), Some(summary)) => {
                format!("Stopped \u{2014} {} counted so far", counted(&summary))
            }
            (Phase::Ready(_), Some(summary)) => {
                let mut text = counted(&summary);
                if self.is_estimate() {
                    text.push_str(" (estimate after cleanup)");
                    if let Some(tree) = self.tree() {
                        text.push_str(&free_space_note(tree.removed_bytes(), &self.free));
                    }
                }
                if summary.incomplete {
                    text.push_str(" \u{b7} some folders could not be read");
                }
                if self.stale {
                    text.push_str(" \u{b7} may have changed since the analysis");
                }
                text
            }
            _ => String::new(),
        }
    }

    /// The footer line of the usage view: the status, and while measuring,
    /// how many children are done because the final total is not known yet.
    pub(crate) fn status_line(&self) -> String {
        let mut text = self.status_text();
        if self.is_walking() {
            let rows = self.rows();
            let done = rows.iter().filter(|row| !row.pending).count();
            text.push_str(&format!(
                " \u{b7} {done} of {} children measured",
                rows.len()
            ));
        }
        text
    }

    /// The rows of the list, in order: the shown folder's children ranked,
    /// with the opened folders' children under them.
    pub(crate) fn rows(&self) -> Vec<ShownRow> {
        match &self.phase {
            Phase::Ready(tree) => {
                let Some(id) = self.shown_id() else {
                    return Vec::new();
                };
                let mut out = Vec::new();
                flatten(tree, id, tree.total(id), &self.expanded, 0, &mut out);
                out
            }
            Phase::Walking(partial) | Phase::Stopped(partial) => {
                let walking = matches!(self.phase, Phase::Walking(_));
                let total = partial.bytes.max(1);
                let mut rows: Vec<ShownRow> = partial
                    .ranked()
                    .iter()
                    .map(|row| partial_row(&self.shown, row, total, walking))
                    .collect();
                if partial.other_files > 0 {
                    rows.push(ShownRow {
                        path: self.shown.clone(),
                        name: format!("{} other files", partial.other_files),
                        kind: ShownKind::Other,
                        bytes: partial.other_bytes,
                        items: partial.other_files,
                        incomplete: false,
                        share: share(partial.other_bytes, total),
                        depth: 0,
                        expandable: false,
                        expanded: false,
                        pending: false,
                    });
                    rows.sort_by_key(|row| std::cmp::Reverse(row.bytes));
                }
                rows
            }
            Phase::Ask | Phase::Failed(_) => Vec::new(),
        }
    }

    /// What activating the item at `path` does.
    pub(crate) fn activation(&self, path: &Path) -> Activation {
        let Some(tree) = self.tree() else {
            return Activation::None;
        };
        if tree.find(path).is_some() {
            return if path == self.analysis_root {
                Activation::None
            } else {
                Activation::Drill(path.to_path_buf())
            };
        }
        let is_file = path
            .parent()
            .and_then(|parent| tree.find(parent))
            .is_some_and(|parent| {
                tree.rows(parent)
                    .iter()
                    .any(|row| row.path == path && matches!(row.item, Item::File { .. }))
            });
        if is_file {
            Activation::Open(path.to_path_buf())
        } else {
            Activation::None
        }
    }

    /// Where Up goes inside the analysis (the parent of the shown folder),
    /// `None` at its root, where Up is the plain parent folder.
    pub(crate) fn up_target(&self) -> Option<PathBuf> {
        if self.shown == self.analysis_root {
            return None;
        }
        self.shown
            .parent()
            .filter(|parent| parent.starts_with(&self.analysis_root))
            .map(Path::to_path_buf)
    }

    /// Takes the removed `paths` out of the totals (see
    /// [`UsageTree::remove`]) and returns how many were held. The numbers
    /// become an estimate: they are the measured sizes, not what the removal
    /// freed.
    pub(crate) fn removed(&mut self, paths: &[PathBuf]) -> usize {
        let Phase::Ready(tree) = &mut self.phase else {
            return 0;
        };
        let subtracted = paths
            .iter()
            .filter(|path| tree.remove(path).is_some())
            .count();
        if subtracted > 0 {
            if self
                .selected
                .as_ref()
                .is_some_and(|selected| paths.iter().any(|path| selected.starts_with(path)))
            {
                self.selected = None;
            }
            self.expanded
                .retain(|open| !paths.iter().any(|path| open.starts_with(path)));
            self.drop_other();
            self.refresh_chart();
        }
        subtracted
    }

    /// The segment of the chart that stands for the selected item.
    pub(crate) fn selected_segment(&self) -> Option<usize> {
        let tree = self.tree()?;
        self.segments
            .iter()
            .position(|segment| match (segment.target, &self.selected) {
                (Target::Item(item), Some(selected)) => item_path(tree, item) == *selected,
                (Target::Other { dir, .. }, None) => self.other == Some(dir),
                _ => false,
            })
    }
}

/// The path an item stands for.
pub(crate) fn item_path(tree: &UsageTree, item: Item) -> PathBuf {
    match item {
        Item::Folder(id) | Item::OtherFiles(id) => tree.path_of(id),
        Item::File { dir, index } => tree
            .items(dir)
            .into_iter()
            .find(|(candidate, _)| *candidate == Item::File { dir, index })
            .map(|(candidate, bytes)| tree.row(candidate, bytes, 0).path)
            .unwrap_or_default(),
    }
}

/// What the list says in place of numbers while the user is asked.
const ASK_NOTICE: &str = "Not measured: a network or phone mount can be slow and wake the device";

/// Whether measuring `path` waits for the user's go-ahead: the details
/// panel's decision for its size (see [`du_auto_allowed`]), so both ask on
/// the same folders. `confirmed` is the folder the user said to go on with.
pub(crate) fn needs_confirmation(
    path: &Path,
    gvfs_root: Option<&Path>,
    confirmed: Option<&Path>,
) -> bool {
    !du_auto_allowed(path, gvfs_root, confirmed)
}

/// The tail of the status line after a cleanup: the estimated freed bytes
/// against what the volume reports free now (and the change since the
/// analysis). Empty until the volume has answered.
pub(crate) fn free_space_note(freed: u64, free: &VolumeFree) -> String {
    let Some(now) = free.now else {
        return String::new();
    };
    let mut note = format!(
        " \u{b7} {} estimated, {} free on the volume",
        format_bytes(freed),
        format_bytes(now)
    );
    if let Some(before) = free.baseline {
        let change = match now.cmp(&before) {
            std::cmp::Ordering::Greater => format!("{} more", format_bytes(now - before)),
            std::cmp::Ordering::Equal => "no change".to_owned(),
            std::cmp::Ordering::Less => format!("{} less", format_bytes(before - now)),
        };
        note.push_str(&format!(" ({change} since the analysis)"));
    }
    note
}

fn items_label(count: u64) -> String {
    if count == 1 {
        "1 item".to_owned()
    } else {
        format!("{count} items")
    }
}

fn share(bytes: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (bytes as f64 / total as f64).min(1.0) as f32
    }
}

fn partial_row(shown: &Path, row: &PartialRow, total: u64, walking: bool) -> ShownRow {
    ShownRow {
        path: shown.join(&row.name),
        name: row.name.to_string_lossy().into_owned(),
        kind: if row.is_folder {
            ShownKind::Folder
        } else {
            ShownKind::File
        },
        bytes: row.bytes,
        items: row.items,
        incomplete: row.incomplete,
        share: share(row.bytes, total),
        depth: 0,
        expandable: false,
        expanded: false,
        pending: walking && !row.done,
    }
}

fn shown_row(row: Row, depth: usize, expanded: &BTreeSet<PathBuf>) -> ShownRow {
    let kind = match row.item {
        Item::Folder(_) => ShownKind::Folder,
        Item::File { .. } => ShownKind::File,
        Item::OtherFiles(_) => ShownKind::Other,
    };
    ShownRow {
        name: row.name.to_string_lossy().into_owned(),
        kind,
        bytes: row.bytes,
        items: row.items,
        incomplete: row.incomplete,
        share: row.share,
        depth,
        expandable: kind == ShownKind::Folder && row.items > 0,
        expanded: expanded.contains(&row.path),
        pending: false,
        path: row.path,
    }
}

fn flatten(
    tree: &UsageTree,
    dir: fileflux::core::usage::NodeId,
    share_of: u64,
    expanded: &BTreeSet<PathBuf>,
    depth: usize,
    out: &mut Vec<ShownRow>,
) {
    for (item, bytes) in tree.items(dir) {
        if out.len() >= MAX_LIST_ROWS {
            return;
        }
        let row = shown_row(tree.row(item, bytes, share_of), depth, expanded);
        let open = row.expandable && row.expanded;
        out.push(row);
        if let (true, Item::Folder(child)) = (open, item) {
            flatten(tree, child, share_of, expanded, depth + 1, out);
        }
    }
}

// --- menus ----------------------------------------------------------------------

/// The menu of a row of the list or a chart segment: the folder or file it
/// stands for, opened, shown where it lives, copied, trashed or deleted.
pub(crate) fn usage_menu_rows() -> Vec<MenuRow> {
    let item = |command, label, shortcut, danger| {
        MenuRow::Item(MenuItemSpec {
            shortcut,
            danger,
            ..MenuItemSpec::new(command, label)
        })
    };
    vec![
        item(MenuCommand::Open, "Open", Some("Enter"), false),
        item(
            MenuCommand::OpenContainingFolder,
            "Open containing folder",
            None,
            false,
        ),
        MenuRow::Separator,
        item(
            MenuCommand::CopyPath,
            "Copy path",
            Some("Ctrl+Shift+C"),
            false,
        ),
        MenuRow::Separator,
        item(MenuCommand::MoveToTrash, "Move to Trash", Some("Del"), true),
        item(
            MenuCommand::DeletePermanently,
            "Delete permanently\u{2026}",
            Some("Shift+Del"),
            true,
        ),
    ]
}

/// Where "Analyze Disk Usage" is offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnalyzeOffer {
    /// On the empty space of a real folder.
    pub(crate) folder: bool,
    /// On the one selected folder.
    pub(crate) selected_folder: bool,
}

/// What a menu offers: only a real folder (not the Trash, Recent, search
/// results or another usage view) has a folder to measure, either itself
/// (nothing selected) or the one selected folder.
pub(crate) fn analyze_offer(
    in_folder: bool,
    selected: usize,
    folder_selected: bool,
) -> AnalyzeOffer {
    AnalyzeOffer {
        folder: in_folder && selected == 0,
        selected_folder: in_folder && selected == 1 && folder_selected,
    }
}

/// Adds the row: at the end of the empty-space menu, and before the first
/// separator of a selected folder's menu (with its open-in rows).
pub(crate) fn add_analyze_row(rows: &mut Vec<MenuRow>, offer: AnalyzeOffer) {
    if !offer.folder && !offer.selected_folder {
        return;
    }
    let row = MenuRow::Item(MenuItemSpec::new(
        MenuCommand::AnalyzeDiskUsage,
        ANALYZE_LABEL,
    ));
    if offer.folder {
        rows.push(row);
    } else {
        let at = rows
            .iter()
            .position(|row| matches!(row, MenuRow::Separator))
            .unwrap_or(rows.len());
        rows.insert(at, row);
    }
}

// --- the explorer ------------------------------------------------------------------

/// The walk of a tab's session: dropping it cancels the walk.
pub(crate) struct UsageRun {
    cancel: Arc<AtomicBool>,
    _task: Task<()>,
}

impl Drop for UsageRun {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl UsageRun {
    pub(crate) fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Explorer {
    /// The active tab's session, while it shows a disk usage view.
    pub(crate) fn usage(&self) -> Option<&UsageSession> {
        let tab = self.tab();
        matches!(tab.state.current(), Location::Usage { .. })
            .then_some(tab.usage.as_ref())
            .flatten()
    }

    /// The item selected in the active tab's finished analysis.
    pub(crate) fn usage_selected_path(&self) -> Option<PathBuf> {
        let session = self.usage()?;
        session.tree()?;
        session.selected.clone()
    }

    /// Up inside a finished analysis goes to the parent folder's usage.
    pub(crate) fn usage_up_transition(&self, transition: Transition) -> Transition {
        if transition == Transition::Up
            && let Some(parent) = self.usage().and_then(UsageSession::up_target)
        {
            return Transition::Visit(Location::Usage { root: parent });
        }
        transition
    }

    /// Brings tab `id`'s session in line with the location it committed:
    /// none off a usage view, a walk, or the cached folder.
    pub(crate) fn sync_usage(&mut self, id: TabId, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        let Location::Usage { root } = tab.state.current().clone() else {
            tab.usage = None;
            tab.usage_run = None;
            return;
        };
        match plan(tab.usage.as_ref(), &root) {
            Plan::Keep => {}
            Plan::Reuse => {
                if let Some(session) = tab.usage.as_mut() {
                    session.show(&root);
                }
            }
            Plan::Walk => {
                let gvfs = gvfs_root_from_env();
                if needs_confirmation(&root, gvfs.as_deref(), tab.usage_confirmed.as_deref()) {
                    tab.usage = Some(UsageSession::asking(root));
                    tab.usage_run = None;
                } else {
                    self.start_usage(id, root, cx);
                }
            }
        }
        cx.notify();
    }

    /// "Measure anyway" on a network or phone mount: remembers the go-ahead
    /// for that folder and starts the walk.
    pub(crate) fn confirm_usage(&mut self, cx: &mut Context<Self>) {
        let id = self.active_id();
        let Some(root) = self
            .usage()
            .filter(|session| session.is_asking())
            .map(|session| session.analysis_root.clone())
        else {
            return;
        };
        self.tab_mut().usage_confirmed = Some(root.clone());
        self.start_usage(id, root, cx);
        cx.notify();
    }

    /// Reads the free space of the volume holding tab `id`'s analysis root
    /// off the UI thread, the way the sidebar meter does (`statvfs`), and
    /// stores it (`after_cleanup` tells which reading it is). A gvfs mount
    /// is not read: its `statvfs` can hang on a sleeping device.
    fn read_volume_free(&mut self, id: TabId, after_cleanup: bool, cx: &mut Context<Self>) {
        let Some(session) = self.tabs.by_id_mut(id).and_then(|tab| tab.usage.as_mut()) else {
            return;
        };
        let root = session.analysis_root.clone();
        let gvfs = gvfs_root_from_env();
        if needs_confirmation(&root, gvfs.as_deref(), None) {
            return;
        }
        let ticket = session.next_free_ticket();
        let read = cx
            .background_executor()
            .spawn(async move { volume_usage(&root).map(|usage| usage.free) });
        cx.spawn(async move |this, cx| {
            let free = read.await;
            let reading = if after_cleanup {
                FreeReading::Now(free)
            } else {
                FreeReading::Before(free)
            };
            if let Err(error) = this.update(cx, |this, cx| {
                let stored = this
                    .tabs
                    .by_id_mut(id)
                    .and_then(|tab| tab.usage.as_mut())
                    .is_some_and(|session| session.accept_free(ticket, reading));
                if stored {
                    cx.notify();
                }
            }) {
                log::debug!("the explorer closed before the free space was read: {error}");
            }
        })
        .detach();
    }

    /// Measures `root` for tab `id` off the UI thread, streaming the totals.
    fn start_usage(&mut self, id: TabId, root: PathBuf, cx: &mut Context<Self>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(UsageProgress::default());
        let walker = {
            let (root, cancel, progress) =
                (root.clone(), Arc::clone(&cancel), Arc::clone(&progress));
            cx.background_executor()
                .spawn(async move { analyze(&root, &cancel, &progress) })
        };
        let flag = Arc::clone(&cancel);
        let task = cx.spawn(async move |this, cx| {
            let current = |this: &Self| {
                this.tabs
                    .iter()
                    .find(|tab| tab.id == id)
                    .and_then(|tab| tab.usage_run.as_ref())
                    .is_some_and(|run| Arc::ptr_eq(&run.cancel, &flag))
            };
            while !progress.is_finished() {
                cx.background_executor().timer(USAGE_TICK).await;
                let partial = progress.snapshot();
                let alive = this.update(cx, |this, cx| {
                    if current(this)
                        && let Some(session) =
                            this.tabs.by_id_mut(id).and_then(|tab| tab.usage.as_mut())
                    {
                        session.progress(partial);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            let result = walker.await;
            let last = progress.snapshot();
            if let Err(error) = this.update(cx, |this, cx| {
                if !current(this) {
                    return;
                }
                let Some(tab) = this.tabs.by_id_mut(id) else {
                    return;
                };
                tab.usage_run = None;
                let Some(session) = tab.usage.as_mut() else {
                    return;
                };
                match result {
                    Ok(UsageEnd::Done(tree)) => {
                        session.finish(tree);
                        this.read_volume_free(id, false, cx);
                    }
                    Ok(UsageEnd::Cancelled) => {
                        session.progress(last);
                        session.stop();
                    }
                    Err(error) => {
                        log::debug!(
                            "could not analyze {}: {error}",
                            session.analysis_root.display()
                        );
                        session.fail(error.to_string());
                    }
                }
                cx.notify();
            }) {
                log::debug!("the explorer closed before the analysis settled: {error}");
            }
        });
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.usage = Some(UsageSession::walking(root));
            tab.usage_run = Some(UsageRun {
                cancel,
                _task: task,
            });
        }
    }

    /// The Stop button: ends the walk, keeping the totals counted so far.
    pub(crate) fn stop_usage(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.tab().usage_run.as_ref() {
            run.stop();
        }
        cx.notify();
    }

    /// "Analyze again": measures the analysis root afresh.
    pub(crate) fn reanalyze_usage(&mut self, cx: &mut Context<Self>) {
        let id = self.active_id();
        let Some(root) = self
            .usage()
            .filter(|session| !session.is_asking())
            .map(|session| session.analysis_root.clone())
        else {
            return;
        };
        self.start_usage(id, root, cx);
        cx.notify();
    }

    /// A press on a list row or chart segment: selects it; the second click
    /// of a double click activates it.
    pub(crate) fn usage_click(
        &mut self,
        path: PathBuf,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.entries_focus().focus(window, cx);
        if let Some(session) = self.tab_mut().usage.as_mut() {
            session.select(&path);
        }
        if click_count >= 2 {
            self.usage_activate(&path, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn usage_toggle(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(session) = self.tab_mut().usage.as_mut() {
            session.toggle(path);
        }
        cx.notify();
    }

    /// Enter on the selection, a double click, a chart click: a folder is
    /// drilled into, a file opens as usual.
    pub(crate) fn usage_activate(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.usage_activate_in(path, Shown::Here, window, cx);
    }

    /// [`Self::usage_activate`] that shows a drilled folder's usage where
    /// `shown` says (a new background tab under Shift); a file opens as ever.
    pub(crate) fn usage_activate_in(
        &mut self,
        path: &Path,
        shown: Shown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let activation = self
            .usage()
            .map_or(Activation::None, |session| session.activation(path));
        match activation {
            Activation::Drill(folder) => {
                self.show_location(Location::Usage { root: folder }, shown, window, cx);
            }
            Activation::Open(file) => self.open_file(&file, cx),
            Activation::None => {}
        }
    }

    /// An arrow key in the entries area while a usage view shows: moves the
    /// chart cursor (see [`UsageSession::move_cursor`]) and shows the
    /// selection in the list. `true` when a usage view has the key, whether
    /// or not the cursor could move.
    pub(crate) fn usage_key(&mut self, movement: Move, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.tab_mut().usage.as_mut() else {
            return false;
        };
        if session.move_cursor(movement) {
            session.reveal_selection();
            cx.notify();
        }
        true
    }

    /// A press on slice `index` of the chart: selects what it stands for.
    pub(crate) fn usage_select_segment(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.entries_focus().focus(window, cx);
        if let Some(session) = self.tab_mut().usage.as_mut()
            && session.select_segment(index)
        {
            session.reveal_selection();
        }
        cx.notify();
    }

    /// Ctrl+= / Ctrl+- or the wheel over the chart: more or fewer rings.
    /// `true` when a usage view has the gesture.
    pub(crate) fn usage_zoom(&mut self, delta: i32, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.tab_mut().usage.as_mut() else {
            return false;
        };
        if session.zoom_rings(delta) {
            cx.notify();
        }
        true
    }

    /// Backspace in a usage view: Up, like the Up button. `true` when a
    /// usage view has the key.
    pub(crate) fn usage_backspace(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.usage().is_none() {
            return false;
        }
        self.navigate(Transition::Up, window, cx);
        true
    }

    /// Escape in a usage view that is drilled into a folder: up to its
    /// parent. At the analysis root Escape keeps its usual meaning.
    pub(crate) fn usage_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.usage().and_then(UsageSession::up_target).is_none() {
            return false;
        }
        self.navigate(Transition::Up, window, cx);
        true
    }

    /// Enter in the entries area while a usage view shows; `true` when it
    /// was handled.
    pub(crate) fn usage_open_selected(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.usage().is_none() {
            return false;
        }
        if let Some(path) = self.usage_selected_path() {
            self.usage_activate(&path, window, cx);
        } else if let Some(folder) = self.usage().and_then(UsageSession::other_activation) {
            self.usage_drill(folder, window, cx);
        }
        true
    }

    /// Shows the usage of `folder`: instant inside the analysis, a new walk
    /// outside it (see [`plan`]).
    pub(crate) fn usage_drill(
        &mut self,
        folder: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate(
            Transition::Visit(Location::Usage { root: folder }),
            window,
            cx,
        );
    }

    /// The menu of the item at `path`, which is selected first.
    pub(crate) fn open_usage_menu(
        &mut self,
        position: Point<Pixels>,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.tab_mut().usage.as_mut() else {
            return;
        };
        if session.tree().is_none() {
            return;
        }
        session.select(path);
        let title = path
            .file_name()
            .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
            .into_owned();
        self.show_menu(
            position,
            usage_menu_rows(),
            (MenuOn::Entries, Some(title)),
            window,
            cx,
        );
    }

    /// A job finished: the removed items leave the totals of every usage
    /// view (marked an estimate), and any other change under an analysis
    /// root marks it as possibly out of date.
    pub(crate) fn usage_after_job(
        &mut self,
        removed: &[PathBuf],
        affected: Option<&Affected>,
        cx: &mut Context<Self>,
    ) {
        let mut cleaned = Vec::new();
        for tab in self.tabs.iter_mut() {
            let Some(session) = tab.usage.as_mut() else {
                continue;
            };
            let subtracted = session.removed(removed);
            if subtracted > 0 {
                cleaned.push(tab.id);
            }
            if removed.is_empty()
                && affected.is_none_or(|affected| touches_root(affected, &session.analysis_root))
            {
                session.mark_stale();
            } else if subtracted == 0 && !removed.is_empty() {
                // Removed something the analysis does not hold individually
                // (a file folded into a bucket): the totals are out of date.
                if removed
                    .iter()
                    .any(|path| path.starts_with(&session.analysis_root))
                {
                    session.mark_stale();
                }
            }
        }
        // What the volume now reports free, to set beside the estimate.
        for id in cleaned {
            self.read_volume_free(id, true, cx);
        }
    }
}

fn touches_root(affected: &Affected, root: &Path) -> bool {
    match affected {
        Affected::Everything => true,
        Affected::Folders(folders) => folders.iter().any(|folder| {
            matches!(folder, Location::Directory(path) if path.starts_with(root) || root.starts_with(path))
        }),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
#[path = "usage_tests.rs"]
mod tests;
