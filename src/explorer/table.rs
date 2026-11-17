//! The details list, drawn by Bolt's own data grid: an [`EntryRowSource`]
//! adapter over the explorer's snapshot answers [`bolt::data::RowSource`],
//! and the actual [`render_header`] / [`render_rows`] / [`GridOverflow`]
//! paint the columns. No handmade header, cells or horizontal scrolling of
//! its own: header and body read one width source and scroll sideways on
//! one handle, and drag-to-resize goes through Bolt's resize helpers.
//!
//! Bolt's row-number gutter (the left column of every data grid in the
//! family, DBFlux and ReqFlux alike) numbers the visible rows; the explorer
//! keeps that convention.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bolt::components::grid::{
    ColumnResize, ColumnResizeDrag, GridCellCoord, GridColors, GridHost, GridOverflow,
    HeaderResize, clamp_column_width, render_header, render_rows, track_column_resize,
};
use bolt::data::{
    CellAlignment, CellTone, CellView, ColumnKind, ColumnView, RowSource, RowVisualState,
};
use bolt::fonts;
use bolt::tokens::GridMetrics;
use fileflux::core::Entry;
use gpui::{
    App, ClickEvent, Context, InteractiveElement, Modifiers, ParentElement, Pixels, Styled,
    WeakEntity, Window, div, prelude::*, px, uniform_list,
};
use gpui_component::ActiveTheme as _;

use super::entries;
use super::util;
use super::view::{ENTRIES_CONTEXT, Explorer};

/// Explicit column widths of the details list, from the reference design
/// (`minmax(180px,1fr) 120px 100px 170px`), with the Name column floored at
/// an explicit minimum so it never collapses in a narrower pane.
pub(crate) const LIST_DEFAULT_WIDTHS: [f32; 4] = [220.0, 120.0, 100.0, 170.0];
/// The Name column's explicit minimum width.
pub(crate) const LIST_NAME_MIN_WIDTH: f32 = LIST_DEFAULT_WIDTHS[0];

/// The session widths of the details-list columns: a width the user dragged
/// wins over the defaults, and the shared drag state survives across
/// frames. The viewport's sideways offset lives on the explorer's own
/// [`gpui::ScrollHandle`], not here — Bolt's [`GridOverflow`] owns no widths
/// and no offset.
#[derive(Default)]
pub(crate) struct ListWidths {
    drag: ColumnResizeDrag,
    widths: HashMap<usize, f32>,
}

impl gpui::Global for ListWidths {}

impl ListWidths {
    /// Takes the width a drag asked for. Column 0 is the Name column and
    /// never goes below [`LIST_NAME_MIN_WIDTH`]; everything else clamps to
    /// Bolt's [`clamp_column_width`].
    fn apply(&mut self, resize: ColumnResize) {
        let width = if resize.column == 0 {
            clamp_column_width(resize.width).max(LIST_NAME_MIN_WIDTH)
        } else {
            clamp_column_width(resize.width)
        };
        self.widths.insert(resize.column, width);
    }

    /// The drawn widths: dragged ones over `defaults`, with the Name floor
    /// applied. This is the one width source the header and the rows read.
    fn widths_for(&self, defaults: &[f32; 4]) -> [f32; 4] {
        let mut widths = *defaults;
        for (index, width) in widths.iter_mut().enumerate() {
            if let Some(dragged) = self.widths.get(&index) {
                *width = if index == 0 {
                    dragged.max(LIST_NAME_MIN_WIDTH)
                } else {
                    *dragged
                };
            }
        }
        widths
    }
}

/// The total width the grid draws at: Bolt's row-number gutter plus the
/// columns — the same value the header and every row draw with, and the
/// width the [`GridOverflow`] viewport scrolls sideways.
pub(crate) fn list_total_width(widths: &[f32; 4]) -> Pixels {
    px(widths
        .iter()
        .fold(f32::from(GridMetrics::ROW_NUMBER_WIDTH), |total, width| {
            total + width
        }))
}

/// The read side of the details grid: a small adapter that answers Bolt's
/// [`RowSource`] from the explorer's snapshot and the indices the current
/// query leaves visible. It holds no strings of its own — cells are
/// formatted only for the visible rows the grid asks for, and
/// [`EntryRowSource::path_of`] translates a visible row back to the exact
/// raw `Entry` path the state keeps for navigation.
pub(crate) struct EntryRowSource {
    snapshot: Arc<Vec<Entry>>,
    visible: Arc<Vec<usize>>,
    columns: Vec<ColumnView>,
}

impl EntryRowSource {
    pub(crate) fn new(snapshot: Arc<Vec<Entry>>, visible: Arc<Vec<usize>>) -> Self {
        Self {
            snapshot,
            visible,
            columns: vec![
                ColumnView::new("Name", ColumnKind::Text)
                    .with_fixed_width(px(LIST_DEFAULT_WIDTHS[0])),
                ColumnView::new("Type", ColumnKind::Text)
                    .with_fixed_width(px(LIST_DEFAULT_WIDTHS[1])),
                ColumnView::new("Size", ColumnKind::Integer)
                    .with_fixed_width(px(LIST_DEFAULT_WIDTHS[2])),
                ColumnView::new("Modified", ColumnKind::Timestamp)
                    .with_fixed_width(px(LIST_DEFAULT_WIDTHS[3])),
            ],
        }
    }

    /// The raw path of the entry at visible row `row`, or `None` past the
    /// count. The one row-to-path translation the click callbacks and the
    /// selection test both read, so the grid can never select a row the
    /// adapter did not map.
    pub(crate) fn path_of(&self, row: usize) -> Option<&Path> {
        let &index = self.visible.get(row)?;
        let entry = self.snapshot.get(index)?;
        Some(entry.path.as_path())
    }
}

impl RowSource for EntryRowSource {
    fn row_count(&self) -> usize {
        self.visible.len()
    }

    fn column_count(&self) -> usize {
        self.columns.len()
    }

    fn column(&self, index: usize) -> Option<ColumnView> {
        self.columns.get(index).cloned()
    }

    fn cell(&self, row: usize, column: usize) -> Option<CellView> {
        let &index = self.visible.get(row)?;
        let entry = self.snapshot.get(index)?;
        let view = match column {
            0 => CellView::new(
                entry.display_name().into_owned(),
                CellTone::Normal,
                CellAlignment::Left,
            ),
            1 => CellView::new(
                util::format_kind(entry),
                CellTone::Muted,
                CellAlignment::Left,
            ),
            2 => CellView::new(
                util::format_size(entry.size),
                CellTone::Muted,
                CellAlignment::Right,
            ),
            3 => CellView::new(
                util::format_modified(entry.modified),
                CellTone::Muted,
                CellAlignment::Left,
            ),
            _ => return None,
        };
        Some(view)
    }

    fn row_state(&self, _row: usize) -> RowVisualState {
        RowVisualState::Clean
    }
}

/// The details list: Bolt's header row over the virtualized rows, both
/// inside a [`GridOverflow`] viewport that scrolls them sideways together
/// while a pane is narrower than the columns. Selection stays with the
/// state (raw paths); a cell click translates its visible row to the exact
/// entry path and applies the same click semantics as a grid tile.
pub(crate) fn render_list(
    explorer: &Explorer,
    visible: Arc<Vec<usize>>,
    selected: Arc<BTreeSet<PathBuf>>,
    weak: &WeakEntity<Explorer>,
    cx: &mut Context<Explorer>,
) -> impl IntoElement {
    if !cx.has_global::<ListWidths>() {
        cx.set_global(ListWidths::default());
    }
    let drag = cx.global::<ListWidths>().drag.clone();
    let widths = cx.global::<ListWidths>().widths_for(&LIST_DEFAULT_WIDTHS);
    let total = list_total_width(&widths);
    let colors = GridColors::of(cx.theme());
    let null_color = bolt::colors(cx).muted;

    let source = Arc::new(EntryRowSource::new(
        Arc::clone(&explorer.snapshot),
        Arc::clone(&visible),
    ));

    let header = render_header(
        "explorer-list-header",
        source.as_ref(),
        &widths,
        fonts::grid_header_height(cx),
        f32::from(total),
        GridMetrics::FONT,
        GridMetrics::TYPE_FONT,
        &colors,
        Some(HeaderResize {
            drag: &drag,
            hover: bolt::brand::brand(cx).accent_tint.opacity(0.3),
        }),
    );

    let count = visible.len();
    let rows_source = Arc::clone(&source);
    let total_width = f32::from(total);
    let null = null_color;
    let selected_for_rows = Arc::clone(&selected);
    let weak_for_rows = weak.clone();
    let list = uniform_list(
        "explorer-list-rows",
        count,
        move |range, _window: &mut Window, cx: &mut App| {
            let colors = GridColors::of(cx.theme());
            let selected = Arc::clone(&selected_for_rows);
            let is_selected = |coord: GridCellCoord| -> bool {
                rows_source
                    .path_of(coord.row)
                    .is_some_and(|path| selected.contains(path))
            };
            let weak = weak_for_rows.clone();
            let click_source = Arc::clone(&rows_source);
            let on_cell_click = Arc::new(
                move |coord: GridCellCoord,
                      event: &ClickEvent,
                      window: &mut Window,
                      cx: &mut App| {
                    let Some(path) = click_source.path_of(coord.row) else {
                        return;
                    };
                    let modifiers = match event {
                        ClickEvent::Mouse(mouse) => mouse.down.modifiers,
                        _ => Modifiers::default(),
                    };
                    entries::handle_entry_click(
                        &weak,
                        path.to_path_buf(),
                        event.click_count(),
                        modifiers.control,
                        window,
                        cx,
                    );
                },
            );
            let host = GridHost {
                column_widths: &widths,
                active: None,
                editing: None,
                is_selected: &is_selected,
                reference_row: false,
                on_cell_click,
                on_cell_context: Arc::new(|_, _, _, _| {}),
                braces_icon: super::icon(gpui_kit_assets::IconName::Braces),
                chevron_icon: super::icon(gpui_kit_assets::IconName::ChevronRight),
                reference_icon: super::icon(gpui_kit_assets::IconName::Crosshair),
            };
            render_rows(
                range,
                rows_source.as_ref(),
                &host,
                None,
                total_width,
                GridMetrics::FONT,
                GridMetrics::ROW_HEIGHT,
                GridMetrics::HEADER_ICON,
                null,
                &colors,
            )
        },
    )
    .flex_1()
    .min_h_0()
    .track_scroll(&explorer.list_scroll.clone());

    // Header and rows scroll sideways together on the one handle the
    // explorer keeps; while the columns overflow, Bolt's viewport reserves
    // the scrollbar strip so the last row stays reachable.
    let overflow = GridOverflow::new(
        "explorer-list-viewport",
        &explorer.list_hscroll,
        f32::from(total),
    )
    .header(header)
    .rows(list)
    .flex_1()
    .min_h_0()
    .w_full();

    // Resize drag handlers live on the list root so they keep firing while
    // the pointer leaves the narrow handle, exactly as DBFlux's table does.
    track_column_resize(
        div()
            .id("explorer-list")
            .key_context(ENTRIES_CONTEXT)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .font_family(fonts::grid_family(cx)),
        &drag,
        move |resize: ColumnResize, _window: &mut Window, app: &mut App| {
            if !app.has_global::<ListWidths>() {
                app.set_global(ListWidths::default());
            }
            app.global_mut::<ListWidths>().apply(resize);
            app.refresh_windows();
        },
    )
    .child(overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bolt::data::CellContent;
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
    /// columns: cells formatted on demand for the rows the grid actually
    /// asks for, honest `None` past the counts, and the one row-to-path
    /// translation the interactions read.
    #[test]
    fn the_list_source_maps_visible_entries_to_cells_and_widths() -> TestResult {
        let entries = vec![entry("alpha.txt", EntryKind::File), {
            let mut notes = entry("Notes", EntryKind::Directory);
            notes.size = None;
            notes
        }];
        let visible = Arc::new(filtered_indices(&entries, "note"));
        let source = EntryRowSource::new(Arc::new(entries.clone()), Arc::clone(&visible));

        assert_eq!(visible.as_ref(), &vec![1], "the query filters to Notes");
        assert_eq!(source.row_count(), 1);
        assert_eq!(source.column_count(), 4, "Name, Type, Size, Modified");

        let name = source.cell(0, 0).ok_or("the name cell exists")?;
        assert_eq!(
            name.content,
            CellContent::Plain("Notes".into()),
            "the name cell shows the visible entry's display name"
        );
        assert_eq!(name.tone, CellTone::Normal);
        assert_eq!(name.align, CellAlignment::Left);
        let kind = source.cell(0, 1).ok_or("the kind cell exists")?;
        assert_eq!(kind.content, CellContent::Plain("Folder".into()));
        assert_eq!(kind.tone, CellTone::Muted, "metadata draws muted");
        let size = source.cell(0, 2).ok_or("the size cell exists")?;
        assert_eq!(
            size.content,
            CellContent::Plain("\u{2014}".into()),
            "directories show no size"
        );
        assert_eq!(size.align, CellAlignment::Right, "sizes right-align");
        let modified = source.cell(0, 3).ok_or("the modified cell exists")?;
        assert_eq!(
            modified.content,
            CellContent::Plain("\u{2014}".into()),
            "missing metadata is honest"
        );

        assert_eq!(source.cell(1, 0), None, "no row past the count");
        assert_eq!(source.cell(0, 4), None, "no column past the count");
        assert_eq!(source.row_state(1), RowVisualState::Clean);
        assert_eq!(source.row_state(0), RowVisualState::Clean);

        assert_eq!(
            source.path_of(0),
            Some(Path::new("/fixture/Notes")),
            "the visible row translates to the exact raw entry path"
        );
        assert_eq!(source.path_of(1), None, "no path past the count");

        // The adapter answers from the *visible* entries, so a filtered-out
        // entry never becomes a row: alpha.txt is row 0 only without a query.
        let unfiltered = EntryRowSource::new(Arc::new(entries), Arc::new(vec![0, 1]));
        assert_eq!(unfiltered.row_count(), 2);
        assert_eq!(unfiltered.path_of(0), Some(Path::new("/fixture/alpha.txt")));
        Ok(())
    }

    /// One width source for header and body: a dragged width wins, the Name
    /// column never collapses below its explicit minimum, and the total the
    /// viewport scrolls is Bolt's gutter plus the columns.
    #[test]
    fn list_widths_come_from_one_source_with_a_floored_name_minimum() {
        let mut store = ListWidths::default();
        store.apply(ColumnResize {
            column: 0,
            width: 10.0,
        });
        store.apply(ColumnResize {
            column: 1,
            width: 2_000.0,
        });
        let widths = store.widths_for(&LIST_DEFAULT_WIDTHS);
        assert_eq!(widths[0], LIST_NAME_MIN_WIDTH, "Name keeps its minimum");
        assert_eq!(widths[1], 2_000.0, "a dragged width wins");
        assert_eq!(widths[2], LIST_DEFAULT_WIDTHS[2], "undragged keeps default");
        assert_eq!(widths[3], LIST_DEFAULT_WIDTHS[3]);

        assert_eq!(
            list_total_width(&widths),
            px(LIST_NAME_MIN_WIDTH + 2_000.0 + LIST_DEFAULT_WIDTHS[2] + LIST_DEFAULT_WIDTHS[3])
                + GridMetrics::ROW_NUMBER_WIDTH,
            "header and rows scroll on exactly this width"
        );
    }
}
