//! The details list, drawn by the pinned `gpui_component::table::DataTable` —
//! the same shared virtualized table the DBFlux/ReqFlux family uses — with a
//! small [`EntryTableDelegate`] over the explorer's snapshot. No handmade
//! header, cells or scrolling of its own, and **no row-number gutter**: the
//! table answers four plain columns (`Name/Type/Size/Modified`), so nothing
//! numbers the visible rows.
//!
//! Widths are the explorer's business: [`distribute_widths`] spreads the
//! columns over the entries area's measured width so the table fills the
//! pane edge to edge — the Name column absorbs the surplus and always gets
//! the largest share — and keeps the header and body aligned while
//! resizing, collapsing the sidebar or overflowing sideways when the pane
//! is genuinely narrower than the column minima. A width the user dragged
//! is respected afterwards and is never recomputed away.
//!
//! Selection stays with the state (raw paths): the delegate's
//! [`TableDelegate::render_tr`] row carries the actual [`ClickEvent`]
//! through [`entries::handle_entry_click`], so single click selects,
//! Ctrl-click toggles and double click opens — [`TableEvent::SelectRow`]
//! carries no modifiers and the table's internal row selection is off, so
//! it can never swallow or clobber the explorer's multi-selection.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bolt::fonts;
use bolt::primitives::Icon;
use fileflux::core::Entry;
use gpui::{
    App, ClickEvent, Context, InteractiveElement as _, IntoElement, Modifiers, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::h_flex;
use gpui_component::{
    ActiveTheme as _, Sizable as _, Size,
    table::{Column, DataTable, TableDelegate, TableState},
};

use super::entries;
use super::util;
use super::view::Explorer;

/// The details list's columns, in order. No ordinal column: the table has
/// exactly these four, nothing numbering the rows.
pub(crate) const COLUMN_COUNT: usize = 4;
/// The columns' designed widths, from the reference design
/// (`minmax(180px,1fr) 120px 100px 170px`).
pub(crate) const LIST_DEFAULT_WIDTHS: [f32; 4] = [220.0, 120.0, 100.0, 170.0];
/// Each column's explicit minimum: below their sum the table overflows
/// sideways instead of squeezing a column unreadable.
pub(crate) const LIST_MIN_WIDTHS: [f32; 4] = [180.0, 90.0, 90.0, 160.0];
/// The Name column's explicit minimum width.
pub(crate) const LIST_NAME_MIN_WIDTH: f32 = LIST_MIN_WIDTHS[0];
/// How far a recorded drag width may drift from the delegate's applied
/// widths before it counts as a deliberate user change.
pub(crate) const DRAG_EPSILON: f32 = 0.5;

const COLUMN_NAMES: [&str; COLUMN_COUNT] = ["Name", "Type", "Size", "Modified"];

/// A cell's presentation, decided by pure functions so the mapping tests
/// read the same truth the cells render.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct CellSpec {
    pub(crate) text: String,
    /// Metadata draws quiet; the name draws primary.
    pub(crate) muted: bool,
    pub(crate) align: CellAlign,
}

/// Horizontal text alignment of one column.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CellAlign {
    Left,
    Right,
}

/// The leading glyph of one entry, matching the tile grid's classification:
/// a real directory draws the folder glyph, a resolvable symlink the
/// symlink glyph, a broken symlink the broken-link glyph, and every other
/// entry the extension-derived file glyph from [`entries::file_glyph`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Glyph {
    Folder,
    File,
    Symlink,
    BrokenLink,
}

/// The leading glyph of one entry.
pub(crate) fn entry_glyph(entry: &Entry) -> Glyph {
    if entry.is_directory() && !entry.is_symlink {
        Glyph::Folder
    } else if entry.is_symlink && entry.kind == fileflux::core::EntryKind::Symlink {
        Glyph::BrokenLink
    } else if entry.is_symlink {
        Glyph::Symlink
    } else {
        Glyph::File
    }
}

/// The alignment of one column: sizes right-align, everything else reads
/// from the left.
pub(crate) fn column_align(column: usize) -> CellAlign {
    if column == 2 {
        CellAlign::Right
    } else {
        CellAlign::Left
    }
}

/// One column's [`Column`] definition: its width, its explicit minimum, no
/// reordering and no column-selection semantics — the drag-to-resize
/// handles stay.
pub(crate) fn column_definition(column: usize, widths: &[f32; 4]) -> Column {
    let width = widths
        .get(column)
        .copied()
        .unwrap_or(LIST_DEFAULT_WIDTHS[0]);
    let min = LIST_MIN_WIDTHS
        .get(column)
        .copied()
        .unwrap_or(LIST_NAME_MIN_WIDTH);
    let name = COLUMN_NAMES
        .get(column)
        .copied()
        .unwrap_or(COLUMN_NAMES[0])
        .to_owned();
    Column::new(
        COLUMN_KEYS.get(column).copied().unwrap_or(COLUMN_KEYS[0]),
        name,
    )
    .width(px(width))
    .min_width(px(min))
    .movable(false)
    .selectable(false)
    .when(column == 2, |col| col.text_right())
}

const COLUMN_KEYS: [&str; COLUMN_COUNT] = ["name", "kind", "size", "modified"];

/// The widths the table draws at, given the entries area's measured width
/// (when the first frame has not measured yet, `None`) and the widths the
/// user dragged.
///
/// The metadata columns keep their designed widths — spaced across the
/// pane, not crammed left — and the Name column absorbs the whole surplus,
/// so the columns reach both edges with Name holding the largest share.
/// When the pane is genuinely narrower than the columns' minima sum, every
/// column sits at its minimum and the table overflows sideways (header and
/// body scroll together on the table's own shared handle). A dragged width
/// is the user's choice: it is floored at its minimum and never recomputed,
/// even when that leaves a gap or overflows.
pub(crate) fn distribute_widths(available: Option<f32>, dragged: &HashMap<usize, f32>) -> [f32; 4] {
    let mut widths = LIST_DEFAULT_WIDTHS;
    for (column, width) in widths.iter_mut().enumerate() {
        if let Some(dragged) = dragged.get(&column) {
            let min = LIST_MIN_WIDTHS
                .get(column)
                .copied()
                .unwrap_or(LIST_NAME_MIN_WIDTH);
            *width = (*dragged).max(min);
        }
    }
    let Some(available) = available else {
        return widths;
    };

    // The metadata columns take what is left after the Name floor: from
    // their minima (nothing left) up to their designed widths (room to
    // spare), so they stay spaced instead of crammed left. A dragged width
    // holds its ground; the undragged ones share the remaining room.
    let metadata_min: f32 = LIST_MIN_WIDTHS[1..].iter().sum();
    let metadata_span: f32 = LIST_DEFAULT_WIDTHS[1..].iter().sum::<f32>() - metadata_min;
    let name_floor = dragged.get(&0).map_or(LIST_NAME_MIN_WIDTH, |_| {
        widths.first().copied().unwrap_or_default()
    });
    let room = (available - name_floor - metadata_min).clamp(0.0, metadata_span);
    let share = if metadata_span > 0.0 {
        room / metadata_span
    } else {
        0.0
    };
    for ((designed, min), (column, width)) in LIST_DEFAULT_WIDTHS
        .iter()
        .zip(LIST_MIN_WIDTHS.iter())
        .skip(1)
        .zip(widths.iter_mut().enumerate().skip(1))
    {
        if dragged.get(&column).is_none() {
            *width = min + (designed - min) * share;
        }
    }

    // The Name column absorbs whatever is still left, so the table reaches
    // both edges; below the minima sum it holds its floor and the table
    // overflows sideways instead.
    if dragged.get(&0).is_none() {
        let metadata_total: f32 = widths[1..].iter().sum();
        widths[0] = (available - metadata_total).max(LIST_NAME_MIN_WIDTH);
    }
    widths
}

/// The session widths the user dragged, owned by the [`Explorer`] (not a
/// global), so two windows never share column widths.
#[derive(Default)]
pub(crate) struct ListWidths {
    widths: HashMap<usize, f32>,
}

impl ListWidths {
    /// Records one deliberately dragged width.
    pub(crate) fn record(&mut self, column: usize, width: f32) {
        self.widths.insert(column, width);
    }

    /// The dragged widths, read by [`distribute_widths`].
    pub(crate) fn dragged(&self) -> &HashMap<usize, f32> {
        &self.widths
    }
}

/// The read side of the details table: the [`TableDelegate`] that answers
/// the explorer's snapshot. It holds no strings of its own — cells are
/// formatted only for the rows the table asks for — and it translates a
/// visible row back to the exact raw `Entry` path the state keeps for
/// navigation. One delegate per explorer's table entity: no row data
/// leaks across windows.
pub(crate) struct EntryTableDelegate {
    snapshot: Arc<Vec<Entry>>,
    visible: Arc<Vec<usize>>,
    selected: Arc<BTreeSet<PathBuf>>,
    widths: [f32; 4],
    weak: WeakEntity<Explorer>,
}

impl EntryTableDelegate {
    pub(crate) fn new(
        weak: WeakEntity<Explorer>,
        snapshot: Arc<Vec<Entry>>,
        visible: Arc<Vec<usize>>,
    ) -> Self {
        Self {
            snapshot,
            visible,
            selected: Arc::new(BTreeSet::new()),
            widths: LIST_DEFAULT_WIDTHS,
            weak,
        }
    }

    /// Re-points the delegate at a new snapshot and visible set. Cheap: it
    /// swaps two `Arc`s; the table reads rows through it on every render.
    pub(crate) fn set_data(&mut self, snapshot: Arc<Vec<Entry>>, visible: Arc<Vec<usize>>) {
        self.snapshot = snapshot;
        self.visible = visible;
    }

    /// Re-points the delegate at the current selection.
    pub(crate) fn set_selection(&mut self, selected: Arc<BTreeSet<PathBuf>>) {
        self.selected = selected;
    }

    /// Applies recomputed column widths (the caller then calls
    /// [`TableState::refresh`] so the table re-reads them).
    pub(crate) fn set_widths(&mut self, widths: [f32; 4]) {
        self.widths = widths;
    }

    /// The widths currently applied to the table.
    pub(crate) fn widths(&self) -> &[f32; 4] {
        &self.widths
    }

    /// The entry at visible row `row`, or `None` past the count.
    pub(crate) fn entry_at(&self, row: usize) -> Option<&Entry> {
        let &index = self.visible.get(row)?;
        self.snapshot.get(index)
    }

    /// The raw path of the entry at visible row `row`, or `None` past the
    /// count. The one row-to-path translation the click callbacks read, so
    /// the table can never select a row the delegate did not map.
    pub(crate) fn path_of(&self, row: usize) -> Option<&Path> {
        Some(self.entry_at(row)?.path.as_path())
    }

    /// The presentation of the cell at `(row, column)`, or `None` past the
    /// counts. Pure, so the mapping tests read exactly what the cells draw.
    pub(crate) fn cell(&self, row: usize, column: usize) -> Option<CellSpec> {
        let entry = self.entry_at(row)?;
        let spec = match column {
            0 => CellSpec {
                text: entry.display_name().into_owned(),
                muted: false,
                align: CellAlign::Left,
            },
            1 => CellSpec {
                text: util::format_kind(entry),
                muted: true,
                align: CellAlign::Left,
            },
            2 => CellSpec {
                text: util::format_size(entry.size),
                muted: true,
                align: CellAlign::Right,
            },
            3 => CellSpec {
                text: util::format_modified(entry.modified),
                muted: true,
                align: CellAlign::Left,
            },
            _ => return None,
        };
        Some(spec)
    }

    /// Test-only views of the counts the trait methods answer.
    #[cfg(test)]
    fn rows_count_for_tests(&self) -> usize {
        self.visible.len()
    }

    #[cfg(test)]
    fn columns_count_for_tests(&self) -> usize {
        COLUMN_COUNT
    }
}

impl Glyph {
    #[cfg(test)]
    fn icon_name_for_tests(self) -> gpui_kit_assets::IconName {
        self.icon_name()
    }
}

impl TableDelegate for EntryTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        COLUMN_COUNT
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.visible.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        column_definition(col_ix, &self.widths)
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let align = column_align(col_ix);
        let mut head = h_flex().size_full().min_w_0().text_size(px(12.0));
        if align == CellAlign::Right {
            head = head.justify_end();
        }
        if let Some(name) = COLUMN_NAMES.get(col_ix) {
            head = head.child(*name);
        }
        head
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let colors = bolt::colors(cx);
        let Some(spec) = self.cell(row_ix, col_ix) else {
            return div().size_full().into_any_element();
        };
        let text = div()
            .min_w_0()
            .flex_1()
            .truncate()
            .text_size(px(if spec.muted { 12.0 } else { 13.0 }))
            .text_color(if spec.muted {
                colors.muted
            } else {
                colors.foreground
            })
            .child(spec.text);
        let mut cell = h_flex().w_full().min_w_0().items_center();
        if spec.align == CellAlign::Right {
            cell = cell.justify_end();
        }
        if col_ix == 0
            && let Some(entry) = self.entry_at(row_ix)
        {
            let glyph = entry_glyph(entry);
            let color = match glyph {
                Glyph::Folder => colors.info,
                Glyph::BrokenLink => colors.warning,
                Glyph::File | Glyph::Symlink => colors.muted,
            };
            let icon = match glyph {
                Glyph::File => entries::file_glyph(&entry.name),
                glyph => glyph.icon_name(),
            };
            cell = cell
                .gap(px(6.0))
                .flex_none()
                .child(Icon::new(super::icon(icon)).size(px(14.0)).color(color));
        }
        cell.child(text).into_any_element()
    }

    /// No trailing spacer column: the widths already fill the pane, and a
    /// residual 12 px empty column would open a permanent sideways
    /// scrollbar and a right-edge gap.
    fn render_last_empty_col(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div().h_full().flex_shrink_0()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        let selected = self
            .entry_at(row_ix)
            .is_some_and(|entry| self.selected.contains(&entry.path));
        let weak = self.weak.clone();
        let mut row = div().id(("row", row_ix));
        if selected {
            row = row.bg(cx.theme().tokens.table_active);
        }
        if let Some(path) = self.path_of(row_ix).map(Path::to_path_buf) {
            row = row.on_click(
                move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                    // The actual event's modifiers decide: Ctrl toggles the
                    // row's membership, double click opens. The table's own
                    // selection stays off, so nothing clobbers this.
                    let modifiers = match event {
                        ClickEvent::Mouse(mouse) => mouse.down.modifiers,
                        _ => Modifiers::default(),
                    };
                    entries::handle_entry_click(
                        &weak,
                        path.clone(),
                        event.click_count(),
                        modifiers.control,
                        window,
                        cx,
                    );
                },
            );
        }
        row
    }
}

impl Glyph {
    /// The Lucide catalog name the glyph draws through Bolt's
    /// [`bolt::icon::IconSource`]. A plain [`Glyph::File`] never reaches
    /// here — its icon comes from [`entries::file_glyph`] instead.
    fn icon_name(self) -> gpui_kit_assets::IconName {
        match self {
            Glyph::Folder => gpui_kit_assets::IconName::Folder,
            Glyph::Symlink => gpui_kit_assets::IconName::FileSymlink,
            Glyph::BrokenLink => gpui_kit_assets::IconName::Link2Off,
            Glyph::File => gpui_kit_assets::IconName::File,
        }
    }
}

/// The details table: the shared virtualized [`DataTable`], edge to edge in
/// the entries area. Column widths are reconciled with the measured entries
/// width and the user's drags before the element is built; the table keeps
/// header and body aligned through its own handles.
pub(crate) fn render_list(
    explorer: &Explorer,
    visible: Arc<Vec<usize>>,
    selected: Arc<BTreeSet<PathBuf>>,
    _weak: &WeakEntity<Explorer>,
    cx: &mut Context<Explorer>,
) -> impl IntoElement {
    let desired = distribute_widths(explorer.entries_width, explorer.list_widths.dragged());
    explorer.table.update(cx, |table, cx| {
        let delegate = table.delegate_mut();
        delegate.set_data(Arc::clone(&explorer.snapshot), Arc::clone(&visible));
        delegate.set_selection(Arc::clone(&selected));
        if delegate.widths() != &desired {
            delegate.set_widths(desired);
            table.refresh(cx);
        }
    });

    let header_height = fonts::grid_header_height(cx);
    div()
        .id("explorer-list")
        .flex_1()
        .min_h_0()
        .w_full()
        .font_family(fonts::ui_family(cx))
        .child(
            DataTable::new(&explorer.table)
                .bordered(false)
                .with_size(Size::Small)
                .header_row_height(header_height),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fileflux::core::EntryKind;
    use fileflux::core::fs::filtered_indices;
    use std::ffi::OsString;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: OsString::from(name),
            path: PathBuf::from(format!("/fixture/{name}")),
            kind,
            is_symlink: false,
            size: Some(10),
            modified: None,
            origin: None,
        }
    }

    /// The source maps the **visible** entries through the four explicit
    /// columns: cells formatted on demand for the rows the table actually
    /// asks for, honest `None` past the counts, and the one row-to-path
    /// translation the interactions read. The name draws primary; the
    /// metadata draws quiet, with sizes right-aligned.
    #[test]
    fn the_list_source_maps_visible_entries_to_cells_and_paths() -> TestResult {
        let entries = vec![entry("alpha.txt", EntryKind::File), {
            let mut notes = entry("Notes", EntryKind::Directory);
            notes.size = None;
            notes
        }];
        let visible = Arc::new(filtered_indices(&entries, "note"));
        let source = EntryTableDelegate::new(
            WeakEntity::<Explorer>::new_invalid(),
            Arc::new(entries.clone()),
            Arc::clone(&visible),
        );

        assert_eq!(visible.as_ref(), &vec![1], "the query filters to Notes");
        assert_eq!(source.rows_count_for_tests(), 1);
        assert_eq!(
            source.columns_count_for_tests(),
            COLUMN_COUNT,
            "Name, Type, Size, Modified — no ordinal column"
        );

        let name = source.cell(0, 0).ok_or("the name cell exists")?;
        assert_eq!(name.text, "Notes", "the name cell shows the display name");
        assert!(!name.muted, "the name draws primary");
        assert_eq!(name.align, CellAlign::Left);
        let kind = source.cell(0, 1).ok_or("the kind cell exists")?;
        assert_eq!(kind.text, "Folder");
        assert!(kind.muted, "metadata draws quiet");
        assert_eq!(kind.align, CellAlign::Left);
        let size = source.cell(0, 2).ok_or("the size cell exists")?;
        assert_eq!(size.text, "\u{2014}", "directories show no size");
        assert!(size.muted);
        assert_eq!(size.align, CellAlign::Right, "sizes right-align");
        let modified = source.cell(0, 3).ok_or("the modified cell exists")?;
        assert_eq!(modified.text, "\u{2014}", "missing metadata is honest");
        assert!(modified.muted);

        assert_eq!(source.cell(1, 0), None, "no row past the count");
        assert_eq!(
            source.cell(0, COLUMN_COUNT),
            None,
            "no column past the count"
        );

        assert_eq!(
            source.path_of(0),
            Some(Path::new("/fixture/Notes")),
            "the visible row translates to the exact raw entry path"
        );
        assert_eq!(source.path_of(1), None, "no path past the count");

        // The delegate answers from the *visible* entries, so a filtered-out
        // entry never becomes a row: alpha.txt is row 0 only without a query.
        let unfiltered = EntryTableDelegate::new(
            WeakEntity::<Explorer>::new_invalid(),
            Arc::new(entries),
            Arc::new(vec![0, 1]),
        );
        assert_eq!(unfiltered.rows_count_for_tests(), 2);
        assert_eq!(unfiltered.path_of(0), Some(Path::new("/fixture/alpha.txt")));
        Ok(())
    }

    /// The leading glyph follows the entry, matching the tile grid's
    /// classification: real directories draw the folder glyph, broken
    /// symlinks the broken-link glyph, resolvable symlinks the symlink
    /// glyph, and files the extension-derived file glyph.
    #[test]
    fn entries_draw_distinct_leading_glyphs() {
        let mut broken = entry("gone", EntryKind::Symlink);
        broken.is_symlink = true;
        assert_eq!(
            entry_glyph(&entry("docs", EntryKind::Directory)),
            Glyph::Folder
        );
        assert_eq!(
            entry_glyph(&entry("notes.txt", EntryKind::File)),
            Glyph::File
        );
        assert_eq!(entry_glyph(&broken), Glyph::BrokenLink);
        let mut linked = entry("docs-link", EntryKind::Directory);
        linked.is_symlink = true;
        assert_eq!(
            entry_glyph(&linked),
            Glyph::Symlink,
            "a resolvable symlink draws its own glyph"
        );
        assert_eq!(
            Glyph::File.icon_name_for_tests(),
            gpui_kit_assets::IconName::File,
            "the file glyph's icon is refined per extension by entries::file_glyph"
        );
    }

    /// The column definitions carry the four names, no reorder, no column
    /// selection, each column's explicit minimum, and the Size column's
    /// right alignment.
    #[test]
    fn column_definitions_carry_names_minima_and_no_ordinal() {
        for (column, name) in ["Name", "Type", "Size", "Modified"].iter().enumerate() {
            let col = column_definition(column, &LIST_DEFAULT_WIDTHS);
            assert_eq!(col.name.as_ref(), *name);
            assert_eq!(
                f32::from(col.width),
                LIST_DEFAULT_WIDTHS.get(column).copied().unwrap_or_default()
            );
            assert_eq!(
                f32::from(col.min_width),
                LIST_MIN_WIDTHS.get(column).copied().unwrap_or_default()
            );
            assert!(!col.movable, "columns never reorder");
            assert!(!col.selectable, "columns never join a selection");
            assert!(col.resizable, "columns stay user-resizable");
            assert_eq!(
                col.align == gpui::TextAlign::Right,
                *name == "Size",
                "only Size right-aligns"
            );
        }
        assert_eq!(COLUMN_COUNT, 4);
    }

    /// One width source for the whole contract: the columns spread over the
    /// measured pane — the Name column absorbing the surplus — metadata
    /// staying spaced, minima holding on narrow panes, and dragged widths
    /// respected.
    #[test]
    fn widths_fill_the_pane_with_the_name_taking_the_surplus() {
        let no_drags = HashMap::new();
        let wide = distribute_widths(Some(1_200.0), &no_drags);
        let total: f32 = wide.iter().sum();
        assert_eq!(total, 1_200.0, "the columns fill the pane exactly");
        assert!(
            wide[0] > wide[1] + wide[2] + wide[3],
            "Name holds the largest share"
        );
        assert_eq!(
            wide[1], LIST_DEFAULT_WIDTHS[1],
            "Type keeps its designed width"
        );
        assert_eq!(
            wide[2], LIST_DEFAULT_WIDTHS[2],
            "Size keeps its designed width"
        );
        assert_eq!(
            wide[3], LIST_DEFAULT_WIDTHS[3],
            "Modified keeps its designed width"
        );

        // The boundary: exactly the minima sum fits; one pixel less overflows.
        let minima_total: f32 = LIST_MIN_WIDTHS.iter().sum();
        let boundary = distribute_widths(Some(minima_total), &no_drags);
        assert_eq!(boundary.iter().sum::<f32>(), minima_total);
        let narrow = distribute_widths(Some(minima_total - 1.0), &no_drags);
        assert_eq!(narrow[0], LIST_NAME_MIN_WIDTH, "Name never collapses");
        assert!(
            narrow.iter().sum::<f32>() > minima_total - 1.0,
            "the table overflows sideways instead of squeezing a column"
        );

        // Before the first measurement, the designed widths apply.
        let first_frame = distribute_widths(None, &no_drags);
        assert_eq!(first_frame, LIST_DEFAULT_WIDTHS);
    }

    /// A width the user dragged is their choice: it is floored at its
    /// minimum and respected afterwards — undragged columns still re-fill.
    #[test]
    fn dragged_widths_are_respected_and_undragged_still_fill() {
        let mut drags = HashMap::new();
        drags.insert(1, 300.0);
        let wide = distribute_widths(Some(1_200.0), &drags);
        assert_eq!(wide[1], 300.0, "a dragged width wins");
        let metadata: f32 = wide[1..].iter().sum();
        assert_eq!(
            wide[0],
            1_200.0 - metadata,
            "the Name column absorbs the surplus the drag left"
        );

        let mut name_drag = HashMap::new();
        name_drag.insert(0, 260.0);
        let named = distribute_widths(Some(1_200.0), &name_drag);
        assert_eq!(named[0], 260.0, "a dragged Name width is respected");
        assert_eq!(named[1..].iter().sum::<f32>(), 390.0, "metadata stay put");

        let mut squeezed = HashMap::new();
        squeezed.insert(0, 10.0);
        let floored = distribute_widths(Some(1_200.0), &squeezed);
        assert_eq!(floored[0], LIST_NAME_MIN_WIDTH, "Name keeps its minimum");
    }

    /// Recorded drag widths stay per-window session state.
    #[test]
    fn list_widths_record_drags_as_session_state() {
        let mut widths = ListWidths::default();
        assert!(widths.dragged().is_empty(), "no drags before any drag");
        widths.record(1, 260.0);
        assert_eq!(widths.dragged().get(&1), Some(&260.0));
        widths.record(1, 240.0);
        assert_eq!(
            widths.dragged().get(&1),
            Some(&240.0),
            "the newest drag wins"
        );
        assert_eq!(
            widths.dragged().get(&0),
            None,
            "undragged columns stay unrecorded"
        );
    }
}
