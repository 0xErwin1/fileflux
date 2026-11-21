//! The details list, drawn by the pinned `gpui_component::table::DataTable` —
//! the same shared virtualized table the DBFlux/ReqFlux family uses — with a
//! small [`EntryTableDelegate`] over the explorer's snapshot. No handmade
//! header, cells or scrolling of its own, and **no row-number gutter**: the
//! table answers three plain columns (`Name/Size/Modified`), so nothing
//! numbers the visible rows.
//!
//! The list's sort ([`SortSpec`]) lives in the explorer state, the single
//! source of truth for the displayed order: a header click sets or flips it
//! there, the sorted header draws strong with a tint arrow, and folders stay
//! grouped before files whatever the key or direction. The delegate only
//! draws the order it is given.
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
use bolt::tokens::ChromeColors;
use fileflux::core::Entry;
use fileflux::core::settings::ColumnPref;
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

use super::actions::RenameView;
use super::arrange::SortKey;
use super::dnd::{self, DropTarget};
use super::entries;
use super::state::SortSpec;
use super::util;
use super::view::Explorer;

/// One column of the details list. Name is always the first; the others
/// are the user's choice (see [`columns_for`]).
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub(crate) enum ListColumn {
    Name,
    Size,
    Modified,
    Type,
    Permissions,
    Owner,
    Group,
}

impl ListColumn {
    /// The column an optional-column preference stands for.
    pub(crate) fn from_pref(pref: ColumnPref) -> Self {
        match pref {
            ColumnPref::Size => Self::Size,
            ColumnPref::Modified => Self::Modified,
            ColumnPref::Type => Self::Type,
            ColumnPref::Permissions => Self::Permissions,
            ColumnPref::Owner => Self::Owner,
            ColumnPref::Group => Self::Group,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Modified => "Modified",
            Self::Type => "Type",
            Self::Permissions => "Permissions",
            Self::Owner => "Owner",
            Self::Group => "Group",
        }
    }

    /// The table's stable id of the column.
    fn table_id(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::Modified => "modified",
            Self::Type => "type",
            Self::Permissions => "permissions",
            Self::Owner => "owner",
            Self::Group => "group",
        }
    }

    /// The designed width, from the reference design for the first three
    /// (a flexible Name then Size and Modified).
    pub(crate) fn default_width(self) -> f32 {
        match self {
            Self::Name => 220.0,
            Self::Size | Self::Owner | Self::Group | Self::Permissions => 110.0,
            Self::Modified => 170.0,
            Self::Type => 150.0,
        }
    }

    /// The explicit minimum: below the sum of these the table overflows
    /// sideways instead of squeezing a column unreadable.
    pub(crate) fn min_width(self) -> f32 {
        match self {
            Self::Name => 180.0,
            Self::Size | Self::Owner | Self::Group => 80.0,
            Self::Modified => 140.0,
            Self::Type => 110.0,
            Self::Permissions => 90.0,
        }
    }

    /// What a header click on the column sorts by.
    pub(crate) fn sorts_by(self) -> SortKey {
        match self {
            Self::Name => SortKey::Name,
            Self::Size => SortKey::Size,
            Self::Modified => SortKey::Modified,
            Self::Type => SortKey::Type,
            Self::Permissions => SortKey::Permissions,
            Self::Owner => SortKey::Owner,
            Self::Group => SortKey::Group,
        }
    }

    /// Sizes right-align, everything else reads from the left.
    pub(crate) fn align(self) -> CellAlign {
        if self == Self::Size {
            CellAlign::Right
        } else {
            CellAlign::Left
        }
    }
}

/// The columns the table is given for the saved optional `prefs`: Name, then
/// each chosen column once, in the canonical order whatever the order of the
/// setting. The default setting yields Name, Size, Modified.
pub(crate) fn columns_for(prefs: &[ColumnPref]) -> Vec<ListColumn> {
    let mut chosen: Vec<ColumnPref> = prefs.to_vec();
    chosen.sort_unstable();
    chosen.dedup();
    std::iter::once(ListColumn::Name)
        .chain(chosen.into_iter().map(ListColumn::from_pref))
        .collect()
}

/// How far a recorded drag width may drift from the delegate's applied
/// widths before it counts as a deliberate user change.
pub(crate) const DRAG_EPSILON: f32 = 0.5;

/// Size of a list row's leading glyph.
const LIST_ROW_GLYPH: f32 = 14.0;

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

/// One column's [`Column`] definition: its width, its explicit minimum, no
/// reordering and no column-selection semantics — the drag-to-resize
/// handles stay.
pub(crate) fn column_definition(column: ListColumn, width: f32) -> Column {
    Column::new(column.table_id(), column.label().to_owned())
        .width(px(width))
        .min_width(px(column.min_width()))
        .movable(false)
        .selectable(false)
        .when(column == ListColumn::Size, |col| col.text_right())
}

/// The widths the table draws `columns` at (Name first), given the entries
/// area's measured width (when the first frame has not measured yet,
/// `None`) and the widths the user dragged.
///
/// The metadata columns keep their designed widths — spaced across the
/// pane, not crammed left — and the Name column absorbs the whole surplus,
/// so the columns reach both edges with Name holding the largest share.
/// When the pane is genuinely narrower than the columns' minima sum, every
/// column sits at its minimum and the table overflows sideways (header and
/// body scroll together on the table's own shared handle). A dragged width
/// is the user's choice: it is floored at its minimum and never recomputed,
/// even when that leaves a gap or overflows.
pub(crate) fn distribute_widths(
    columns: &[ListColumn],
    available: Option<f32>,
    dragged: &HashMap<ListColumn, f32>,
) -> Vec<f32> {
    let mut widths: Vec<f32> = columns
        .iter()
        .map(|column| {
            dragged
                .get(column)
                .map_or(column.default_width(), |dragged| {
                    (*dragged).max(column.min_width())
                })
        })
        .collect();
    let Some(available) = available else {
        return widths;
    };
    let Some((name, metadata)) = columns.split_first() else {
        return widths;
    };

    // The metadata columns take what is left after the Name floor: from
    // their minima (nothing left) up to their designed widths (room to
    // spare), so they stay spaced instead of crammed left. A dragged width
    // holds its ground; the undragged ones share the remaining room.
    let metadata_min: f32 = metadata.iter().map(|column| column.min_width()).sum();
    let metadata_span: f32 = metadata
        .iter()
        .map(|column| column.default_width())
        .sum::<f32>()
        - metadata_min;
    let name_floor = if dragged.contains_key(name) {
        widths.first().copied().unwrap_or_default()
    } else {
        name.min_width()
    };
    let room = (available - name_floor - metadata_min).clamp(0.0, metadata_span);
    let share = if metadata_span > 0.0 {
        room / metadata_span
    } else {
        0.0
    };
    for (column, width) in metadata.iter().zip(widths.iter_mut().skip(1)) {
        if !dragged.contains_key(column) {
            let min = column.min_width();
            *width = min + (column.default_width() - min) * share;
        }
    }

    // The Name column absorbs whatever is still left, so the table reaches
    // both edges; below the minima sum it holds its floor and the table
    // overflows sideways instead.
    if !dragged.contains_key(name) {
        let metadata_total: f32 = widths.iter().skip(1).sum();
        if let Some(first) = widths.first_mut() {
            *first = (available - metadata_total).max(name.min_width());
        }
    }
    widths
}

/// The session widths the user dragged, by column, owned by the
/// [`Explorer`] (not a global), so two windows never share column widths.
/// Keyed by column, not position: hiding another column keeps the width.
#[derive(Default)]
pub(crate) struct ListWidths {
    widths: HashMap<ListColumn, f32>,
}

impl ListWidths {
    /// Records one deliberately dragged width.
    pub(crate) fn record(&mut self, column: ListColumn, width: f32) {
        self.widths.insert(column, width);
    }

    /// The dragged widths, read by [`distribute_widths`].
    pub(crate) fn dragged(&self) -> &HashMap<ListColumn, f32> {
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
    /// The snapshot indices to draw, already in displayed order (the
    /// state's sort has been applied).
    visible: Arc<Vec<usize>>,
    /// The sort the header shows; the state owns it.
    sort: SortSpec,
    selected: Arc<BTreeSet<PathBuf>>,
    /// Entries cut and waiting for Paste, drawn dimmed.
    cut: Arc<BTreeSet<PathBuf>>,
    /// Folder item counts, counted off the UI thread after the scan.
    child_counts: Arc<HashMap<PathBuf, Option<usize>>>,
    columns: Vec<ListColumn>,
    widths: Vec<f32>,
    /// The inline rename editor, drawn in the name cell of its row.
    renaming: Option<RenameView>,
    /// The visible selection a row drag carries; `None` in the Trash view.
    dragged: Option<Arc<Vec<PathBuf>>>,
    /// The selection is what an open context menu acts on.
    outlined: bool,
    /// Each name is followed by the folder its entry lives in (search
    /// results).
    with_folder: bool,
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
            sort: SortSpec::default(),
            selected: Arc::new(BTreeSet::new()),
            cut: Arc::new(BTreeSet::new()),
            child_counts: Arc::new(HashMap::new()),
            columns: columns_for(&ColumnPref::defaults()),
            widths: distribute_widths(&columns_for(&ColumnPref::defaults()), None, &HashMap::new()),
            renaming: None,
            dragged: None,
            outlined: false,
            with_folder: false,
            weak,
        }
    }

    /// Shows the folder of each entry after its name. `true` when it changed.
    pub(crate) fn set_with_folder(&mut self, with_folder: bool) -> bool {
        let changed = self.with_folder != with_folder;
        self.with_folder = with_folder;
        changed
    }

    /// Re-points the delegate at a new snapshot and its visible rows in
    /// displayed order. `true` when anything changed (it runs on every
    /// render), so the caller refreshes the table only then.
    pub(crate) fn set_data(&mut self, snapshot: Arc<Vec<Entry>>, visible: Arc<Vec<usize>>) -> bool {
        if Arc::ptr_eq(&self.snapshot, &snapshot) && self.visible == visible {
            return false;
        }
        self.snapshot = snapshot;
        self.visible = visible;
        true
    }

    /// Points the header at the state's sort; `true` when it changed.
    pub(crate) fn set_sort(&mut self, sort: SortSpec) -> bool {
        let changed = self.sort != sort;
        self.sort = sort;
        changed
    }

    /// Re-points the delegate at the folder counts; `true` when they
    /// changed, so the caller refreshes the table.
    pub(crate) fn set_child_counts(
        &mut self,
        counts: Arc<HashMap<PathBuf, Option<usize>>>,
    ) -> bool {
        let changed = !Arc::ptr_eq(&self.child_counts, &counts);
        self.child_counts = counts;
        changed
    }

    /// Re-points the delegate at the rename editor; `true` when it changed,
    /// so the caller refreshes the table.
    pub(crate) fn set_renaming(&mut self, renaming: Option<RenameView>) -> bool {
        let changed = self.renaming != renaming;
        self.renaming = renaming;
        changed
    }

    /// Re-points the delegate at the current selection.
    /// Sets the entries drawn dimmed. `true` when the set changed.
    pub(crate) fn set_cut(&mut self, cut: BTreeSet<PathBuf>) -> bool {
        if *self.cut == cut {
            return false;
        }
        self.cut = Arc::new(cut);
        true
    }

    pub(crate) fn set_selection(&mut self, selected: Arc<BTreeSet<PathBuf>>) {
        self.selected = selected;
    }

    /// The visible selection rows start a drag with; `None` where entries
    /// cannot be dragged (the Trash view).
    pub(crate) fn set_dragged(&mut self, dragged: Option<Arc<Vec<PathBuf>>>) {
        self.dragged = dragged;
    }

    /// Whether the selected rows are the target of an open context menu.
    pub(crate) fn set_outlined(&mut self, outlined: bool) {
        self.outlined = outlined;
    }

    /// Applies recomputed column widths (the caller then calls
    /// [`TableState::refresh`] so the table re-reads them).
    pub(crate) fn set_widths(&mut self, widths: Vec<f32>) {
        self.widths = widths;
    }

    /// The widths currently applied to the table.
    pub(crate) fn widths(&self) -> &[f32] {
        &self.widths
    }

    /// Points the table at the columns to draw (Name first); `true` when
    /// they changed. The caller then sets the widths of the new columns.
    pub(crate) fn set_columns(&mut self, columns: Vec<ListColumn>) -> bool {
        let changed = self.columns != columns;
        self.columns = columns;
        changed
    }

    /// The columns the table draws, Name first.
    pub(crate) fn columns(&self) -> &[ListColumn] {
        &self.columns
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
        let column = *self.columns.get(column)?;
        let metadata = |text: String| CellSpec {
            text,
            muted: true,
            align: column.align(),
        };
        // Owner and group come from the scan; an entry without them is honest.
        let unix = |read: fn(&fileflux::core::UnixInfo) -> String| {
            entry
                .unix
                .as_ref()
                .map_or_else(|| "\u{2014}".to_owned(), read)
        };
        Some(match column {
            ListColumn::Name => CellSpec {
                text: entry.display_name().into_owned(),
                muted: false,
                align: CellAlign::Left,
            },
            // Folder counts arrive from a background pass after the scan.
            ListColumn::Size => metadata(util::format_entry_size(
                entry,
                self.child_counts.get(&entry.path).copied().flatten(),
            )),
            ListColumn::Modified => {
                metadata(util::format_modified(entry.modified, &chrono::Local::now()))
            }
            // The same label the details panel's subtitle shows.
            ListColumn::Type => metadata(super::inspector::kind_label(entry)),
            ListColumn::Permissions => {
                metadata(unix(|info| super::inspector::permission_string(info.mode)))
            }
            ListColumn::Owner => metadata(unix(|info| info.owner.to_string())),
            ListColumn::Group => metadata(unix(|info| info.group.to_string())),
        })
    }

    /// Test-only views of the counts the trait methods answer.
    #[cfg(test)]
    fn rows_count_for_tests(&self) -> usize {
        self.visible.len()
    }

    #[cfg(test)]
    fn columns_count_for_tests(&self) -> usize {
        self.columns.len()
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
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.visible.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let column = self
            .columns
            .get(col_ix)
            .copied()
            .unwrap_or(ListColumn::Name);
        let width = self
            .widths
            .get(col_ix)
            .copied()
            .unwrap_or_else(|| column.default_width());
        column_definition(column, width)
    }

    /// The header: the column name, strong with a ↑/↓ arrow when it is the
    /// sorted column, quiet otherwise. A click sets or flips the sort — the
    /// delegate owns it, so the table's built-in sort icon stays off.
    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let colors = bolt::colors(cx);
        let tint = ChromeColors::tint(cx.theme());
        let weak = self.weak.clone();
        let column = self
            .columns
            .get(col_ix)
            .copied()
            .unwrap_or(ListColumn::Name);
        let sorted = Some(column.sorts_by()).filter(|key| *key == self.sort.key);
        let mut head = h_flex()
            .id(("sort-header", col_ix))
            .size_full()
            .min_w_0()
            .gap(px(6.0))
            .text_size(px(12.0))
            .text_color(if sorted.is_some() {
                colors.strong
            } else {
                colors.muted
            })
            .hover(|this| this.text_color(colors.foreground))
            .on_click(move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                let key = column.sorts_by();
                if let Err(error) = weak.update(cx, |explorer, cx| {
                    explorer.tab_mut().state.click_sort(key);
                    let id = explorer.active_id();
                    explorer.arrange_tab(id, cx);
                    cx.notify();
                }) {
                    log::debug!("the list header outlived its explorer: {error}");
                }
            })
            // The header's own menu: which columns the list shows.
            .on_mouse_down(gpui::MouseButton::Right, {
                let weak = self.weak.clone();
                move |event: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
                    cx.stop_propagation();
                    if let Err(error) = weak.update(cx, |explorer, cx| {
                        explorer.open_arrange_menu(
                            super::ops::ArrangeMenu::Columns,
                            event.position,
                            window,
                            cx,
                        );
                    }) {
                        log::debug!("the list header outlived its explorer: {error}");
                    }
                }
            });
        if column.align() == CellAlign::Right {
            head = head.justify_end();
        }
        head = head.child(column.label());
        if sorted.is_some() {
            head = head.child(div().text_color(tint).child(self.sort.dir.arrow()));
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
        let editing = (col_ix == 0)
            .then(|| {
                let entry = self.entry_at(row_ix)?;
                self.renaming
                    .as_ref()
                    .filter(|view| view.path == entry.path)
            })
            .flatten()
            .cloned();
        let text = div()
            .min_w_0()
            .flex_1()
            .truncate()
            .when(spec.align == CellAlign::Right, |text| text.text_right())
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
            // A plain file prefers the installed theme's icon for its type;
            // folders, symlinks and broken links keep their Lucide glyphs.
            let icon = match glyph {
                Glyph::File => entries::file_icon(&entry.name, color, LIST_ROW_GLYPH),
                glyph => Icon::new(super::icon(glyph.icon_name()))
                    .size(px(LIST_ROW_GLYPH))
                    .color(color)
                    .into_any_element(),
            };
            cell = cell.gap(px(6.0)).flex_none().child(icon);
        }
        match editing {
            Some(view) => cell
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .child(super::overlay::renamer(&view, &self.weak, false, cx)),
                )
                .into_any_element(),
            None => {
                let folder = self
                    .with_folder
                    .then(|| self.entry_at(row_ix))
                    .flatten()
                    .filter(|_| col_ix == 0)
                    .and_then(|entry| entry.origin.as_deref())
                    .map(|folder| folder.to_string_lossy().into_owned());
                cell.child(text)
                    .children(folder.map(|folder| {
                        div()
                            .min_w_0()
                            .max_w(px(260.0))
                            .pl(px(10.0))
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(colors.muted)
                            .child(folder)
                    }))
                    .into_any_element()
            }
        }
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
        // The lighter divider token between rows, set explicitly so a theme
        // change in the table's own default can never make them heavy again.
        let mut row = div()
            .id(("row", row_ix))
            .border_color(bolt::colors(cx).row_divider);
        if selected {
            row = row.bg(cx.theme().tokens.table_active);
        }
        let tint = ChromeColors::tint(cx.theme());
        if selected && self.outlined {
            // What the open menu acts on: tint wash and a full outline.
            row = row
                .bg(tint.opacity(entries::CONTEXT_WASH_ALPHA))
                .border_1()
                .border_color(tint.opacity(entries::CONTEXT_OUTLINE_ALPHA));
        }
        if self
            .entry_at(row_ix)
            .is_some_and(|entry| self.cut.contains(&entry.path))
        {
            row = row.opacity(entries::CUT_OPACITY);
        }
        if let Some(path) = self.path_of(row_ix).map(Path::to_path_buf) {
            if let Some(entry) = self.entry_at(row_ix)
                && let Some(selection) = &self.dragged
                && !self.renaming.as_ref().is_some_and(|view| view.path == path)
            {
                let folder = entry.is_directory() && !entry.is_symlink;
                let glyph = if folder {
                    gpui_kit_assets::IconName::Folder
                } else {
                    entries::file_glyph(&entry.name)
                };
                row =
                    dnd::drag_source(row, path.clone(), glyph, folder, selected, selection, &weak);
                if folder {
                    row = dnd::drop_target(row, DropTarget::Folder(path.clone()), &weak, cx);
                }
            }
            row = dnd::block_while_dragging(row, cx);
            row = row.on_mouse_down(
                gpui::MouseButton::Right,
                entries::entry_context_menu(&weak, path.clone()),
            );
            row = row.on_mouse_up(
                gpui::MouseButton::Middle,
                entries::entry_middle_click(&weak, path.clone()),
            );
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
                        modifiers,
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
    dragged: Option<Arc<Vec<PathBuf>>>,
    outlined: bool,
    _weak: &WeakEntity<Explorer>,
    cx: &mut Context<Explorer>,
) -> impl IntoElement {
    let columns = columns_for(&explorer.list_columns);
    let desired = distribute_widths(
        &columns,
        explorer.entries_width(),
        explorer.list_widths.dragged(),
    );
    explorer.tab().table.update(cx, |table, cx| {
        let delegate = table.delegate_mut();
        let data_changed =
            delegate.set_data(Arc::clone(&explorer.tab().snapshot), Arc::clone(&visible));
        let sort_changed = delegate.set_sort(explorer.tab().state.sort());
        let counts_changed =
            delegate.set_child_counts(Arc::clone(explorer.tab().state.child_counts()));
        delegate.set_selection(Arc::clone(&selected));
        delegate.set_dragged(dragged);
        delegate.set_outlined(outlined);
        let cut_changed = delegate.set_cut(explorer.clipboard.cut_paths());
        let folder_changed = delegate.set_with_folder(matches!(
            explorer.tab().state.current(),
            fileflux::core::Location::Search { .. }
        ));
        let renaming_changed = delegate.set_renaming(
            explorer
                .renaming
                .as_ref()
                .map(super::actions::Renaming::view),
        );
        let columns_changed = delegate.set_columns(columns);
        let widths_changed = delegate.widths() != desired.as_slice();
        if widths_changed {
            delegate.set_widths(desired);
        }
        if data_changed
            || sort_changed
            || counts_changed
            || widths_changed
            || columns_changed
            || renaming_changed
            || cut_changed
            || folder_changed
        {
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
            DataTable::new(&explorer.tab().table)
                .bordered(false)
                .with_size(Size::Small)
                .header_row_height(header_height),
        )
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::explorer::arrange::{FolderCounts, SortDir, compare_entries};
    use fileflux::core::EntryKind;
    use fileflux::core::fs::filtered_indices;
    use std::ffi::OsString;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// The three columns of before the optional ones, and their widths as
    /// they were designed: nothing chosen must draw exactly this.
    const THREE: [ListColumn; 3] = [ListColumn::Name, ListColumn::Size, ListColumn::Modified];
    const LIST_DEFAULT_WIDTHS: [f32; 3] = [220.0, 110.0, 170.0];
    const LIST_MIN_WIDTHS: [f32; 3] = [180.0, 80.0, 140.0];
    const LIST_NAME_MIN_WIDTH: f32 = 180.0;

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: OsString::from(name),
            path: PathBuf::from(format!("/fixture/{name}")),
            kind,
            is_symlink: false,
            size: Some(10),
            modified: None,
            origin: None,
            unix: None,
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
            THREE.len(),
            "Name, Size, Modified — no ordinal column"
        );

        let name = source.cell(0, 0).ok_or("the name cell exists")?;
        assert_eq!(name.text, "Notes", "the name cell shows the display name");
        assert!(!name.muted, "the name draws primary");
        assert_eq!(name.align, CellAlign::Left);
        let size = source.cell(0, 1).ok_or("the size cell exists")?;
        assert_eq!(size.text, "", "folders without a known count stay empty");
        assert!(size.muted, "metadata draws quiet");
        assert_eq!(size.align, CellAlign::Right, "sizes right-align");
        let modified = source.cell(0, 2).ok_or("the modified cell exists")?;
        assert_eq!(modified.text, "\u{2014}", "missing metadata is honest");
        assert!(modified.muted);

        assert_eq!(source.cell(1, 0), None, "no row past the count");
        assert_eq!(
            source.cell(0, THREE.len()),
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
        // entry never becomes a row: alpha.txt is a row only without a query,
        // and draws the rows in the displayed order the state hands it.
        let unfiltered = EntryTableDelegate::new(
            WeakEntity::<Explorer>::new_invalid(),
            Arc::new(entries),
            Arc::new(vec![1, 0]),
        );
        assert_eq!(unfiltered.rows_count_for_tests(), 2);
        assert_eq!(unfiltered.path_of(0), Some(Path::new("/fixture/Notes")));
        assert_eq!(unfiltered.path_of(1), Some(Path::new("/fixture/alpha.txt")));
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
        for (position, name) in ["Name", "Size", "Modified"].iter().enumerate() {
            let column = THREE[position];
            let col = column_definition(column, LIST_DEFAULT_WIDTHS[position]);
            assert_eq!(col.name.as_ref(), *name);
            assert_eq!(f32::from(col.width), LIST_DEFAULT_WIDTHS[position]);
            assert_eq!(f32::from(col.min_width), LIST_MIN_WIDTHS[position]);
            assert!(!col.movable, "columns never reorder");
            assert!(!col.selectable, "columns never join a selection");
            assert!(col.resizable, "columns stay user-resizable");
            assert_eq!(
                col.align == gpui::TextAlign::Right,
                *name == "Size",
                "only Size right-aligns"
            );
        }
        assert_eq!(
            THREE.map(ListColumn::default_width),
            LIST_DEFAULT_WIDTHS,
            "the designed widths did not move"
        );
        assert_eq!(THREE.map(ListColumn::min_width), LIST_MIN_WIDTHS);
    }

    /// One width source for the whole contract: the columns spread over the
    /// measured pane — the Name column absorbing the surplus — metadata
    /// staying spaced, minima holding on narrow panes, and dragged widths
    /// respected.
    #[test]
    fn widths_fill_the_pane_with_the_name_taking_the_surplus() {
        let no_drags = HashMap::new();
        let wide = distribute_widths(&THREE, Some(1_200.0), &no_drags);
        let total: f32 = wide.iter().sum();
        assert_eq!(total, 1_200.0, "the columns fill the pane exactly");
        assert!(wide[0] > wide[1] + wide[2], "Name holds the largest share");
        assert_eq!(
            wide[1], LIST_DEFAULT_WIDTHS[1],
            "Size keeps its designed width"
        );
        assert_eq!(
            wide[2], LIST_DEFAULT_WIDTHS[2],
            "Modified keeps its designed width"
        );

        // The boundary: exactly the minima sum fits; one pixel less overflows.
        let minima_total: f32 = LIST_MIN_WIDTHS.iter().sum();
        let boundary = distribute_widths(&THREE, Some(minima_total), &no_drags);
        assert_eq!(boundary.iter().sum::<f32>(), minima_total);
        let narrow = distribute_widths(&THREE, Some(minima_total - 1.0), &no_drags);
        assert_eq!(narrow[0], LIST_NAME_MIN_WIDTH, "Name never collapses");
        assert!(
            narrow.iter().sum::<f32>() > minima_total - 1.0,
            "the table overflows sideways instead of squeezing a column"
        );

        // Before the first measurement, the designed widths apply.
        let first_frame = distribute_widths(&THREE, None, &no_drags);
        assert_eq!(first_frame, LIST_DEFAULT_WIDTHS);
    }

    /// A width the user dragged is their choice: it is floored at its
    /// minimum and respected afterwards — undragged columns still re-fill.
    #[test]
    fn dragged_widths_are_respected_and_undragged_still_fill() {
        let mut drags = HashMap::new();
        drags.insert(ListColumn::Size, 300.0);
        let wide = distribute_widths(&THREE, Some(1_200.0), &drags);
        assert_eq!(wide[1], 300.0, "a dragged width wins");
        let metadata: f32 = wide[1..].iter().sum();
        assert_eq!(
            wide[0],
            1_200.0 - metadata,
            "the Name column absorbs the surplus the drag left"
        );

        let mut name_drag = HashMap::new();
        name_drag.insert(ListColumn::Name, 260.0);
        let named = distribute_widths(&THREE, Some(1_200.0), &name_drag);
        assert_eq!(named[0], 260.0, "a dragged Name width is respected");
        assert_eq!(named[1..].iter().sum::<f32>(), 280.0, "metadata stay put");

        let mut squeezed = HashMap::new();
        squeezed.insert(ListColumn::Name, 10.0);
        let floored = distribute_widths(&THREE, Some(1_200.0), &squeezed);
        assert_eq!(floored[0], LIST_NAME_MIN_WIDTH, "Name keeps its minimum");
    }

    fn sized(name: &str, kind: EntryKind, size: Option<u64>, secs: Option<u64>) -> Entry {
        let mut e = entry(name, kind);
        e.size = size;
        e.modified = secs.map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
        e
    }

    fn names(entries: &[Entry], order: &[usize]) -> Vec<String> {
        order
            .iter()
            .filter_map(|&i| entries.get(i))
            .map(|e| e.display_name().into_owned())
            .collect()
    }

    fn fixture() -> Vec<Entry> {
        vec![
            sized("beta.txt", EntryKind::File, Some(300), Some(200)),
            sized("Zeta", EntryKind::Directory, None, Some(50)),
            sized("alpha.txt", EntryKind::File, Some(900), None),
            sized("Docs", EntryKind::Directory, None, Some(500)),
            sized("gamma.txt", EntryKind::File, None, Some(100)),
        ]
    }

    fn spec(key: SortKey, dir: SortDir) -> SortSpec {
        SortSpec {
            key,
            dir,
            ..SortSpec::default()
        }
    }

    /// The `visible` indices reordered under `spec`, the way the state does.
    fn sorted_indices(entries: &[Entry], visible: &[usize], spec: SortSpec) -> Vec<usize> {
        let counts = FolderCounts::new();
        let mut order = visible.to_vec();
        order.sort_by(|&a, &b| match (entries.get(a), entries.get(b)) {
            (Some(a), Some(b)) => compare_entries(a, b, spec, &counts),
            (a, b) => a.is_some().cmp(&b.is_some()).reverse(),
        });
        order
    }

    /// Folders stay grouped before files under every key and direction;
    /// the key orders within each group, the direction reverses only that,
    /// and a missing size or time sorts lowest.
    #[test]
    fn sorting_groups_folders_first_under_every_key_and_direction() {
        let entries = fixture();
        let all: Vec<usize> = (0..entries.len()).collect();
        let order = |key, dir| names(&entries, &sorted_indices(&entries, &all, spec(key, dir)));

        assert_eq!(
            order(SortKey::Name, SortDir::Ascending),
            ["Docs", "Zeta", "alpha.txt", "beta.txt", "gamma.txt"],
            "name ascending is case-insensitive"
        );
        assert_eq!(
            order(SortKey::Name, SortDir::Descending),
            ["Zeta", "Docs", "gamma.txt", "beta.txt", "alpha.txt"],
            "descending still keeps folders first"
        );
        assert_eq!(
            order(SortKey::Size, SortDir::Ascending),
            ["Docs", "Zeta", "gamma.txt", "beta.txt", "alpha.txt"],
            "unknown size sorts lowest; equal folders tie by name"
        );
        assert_eq!(
            order(SortKey::Size, SortDir::Descending),
            ["Docs", "Zeta", "alpha.txt", "beta.txt", "gamma.txt"],
            "size descending: folders (no size) tie by name ascending"
        );
        assert_eq!(
            order(SortKey::Modified, SortDir::Ascending),
            ["Zeta", "Docs", "alpha.txt", "gamma.txt", "beta.txt"],
            "unknown time sorts lowest"
        );
        assert_eq!(
            order(SortKey::Modified, SortDir::Descending),
            ["Docs", "Zeta", "beta.txt", "gamma.txt", "alpha.txt"]
        );
    }

    /// Only the visible indices are reordered, and an out-of-range index
    /// never panics.
    #[test]
    fn sorting_reorders_only_the_visible_indices() {
        let entries = fixture();
        let order = sorted_indices(&entries, &[4, 0, 99], SortSpec::default());
        assert_eq!(order.get(..2), Some(&[0, 4][..]), "beta before gamma");
        assert_eq!(order.len(), 3, "nothing is dropped or invented");
    }

    /// A click on the active key flips its direction; another key starts
    /// ascending. Unknown columns map to no key.
    #[test]
    fn header_clicks_set_or_flip_the_sort() {
        let name_asc = spec(SortKey::Name, SortDir::Ascending);
        assert_eq!(
            SortSpec::default(),
            spec(SortKey::Modified, SortDir::Descending),
            "the default sort is the newest first"
        );
        assert_eq!(
            name_asc.clicked(SortKey::Name),
            spec(SortKey::Name, SortDir::Descending)
        );
        assert_eq!(
            name_asc.clicked(SortKey::Size),
            spec(SortKey::Size, SortDir::Ascending)
        );
        assert_eq!(
            name_asc.clicked(SortKey::Size).clicked(SortKey::Size).dir,
            SortDir::Descending
        );
        assert_eq!(SortDir::Ascending.arrow(), "\u{2191}");
        assert_eq!(SortDir::Descending.arrow(), "\u{2193}");
        assert_eq!(
            THREE.map(ListColumn::sorts_by),
            [SortKey::Name, SortKey::Size, SortKey::Modified]
        );
    }

    /// Recorded drag widths stay per-window session state.
    #[test]
    fn list_widths_record_drags_as_session_state() {
        let mut widths = ListWidths::default();
        assert!(widths.dragged().is_empty(), "no drags before any drag");
        widths.record(ListColumn::Size, 260.0);
        assert_eq!(widths.dragged().get(&ListColumn::Size), Some(&260.0));
        widths.record(ListColumn::Size, 240.0);
        assert_eq!(
            widths.dragged().get(&ListColumn::Size),
            Some(&240.0),
            "the newest drag wins"
        );
        assert_eq!(
            widths.dragged().get(&ListColumn::Name),
            None,
            "undragged columns stay unrecorded"
        );
    }

    use fileflux::core::UnixInfo;

    fn owned(name: &str, mode: u32, owner: &str, group: &str) -> Entry {
        let mut e = entry(name, EntryKind::File);
        e.unix = Some(UnixInfo {
            mode,
            uid: 1000,
            gid: 100,
            owner: owner.into(),
            group: group.into(),
        });
        e
    }

    /// The setting becomes the table's columns: Name first, each chosen
    /// column once, in the canonical order; the default setting is today's
    /// three columns.
    #[test]
    fn the_columns_given_to_the_table_follow_the_setting() {
        use ColumnPref::{Group, Modified, Owner, Permissions, Size, Type};
        assert_eq!(columns_for(&ColumnPref::defaults()), THREE.to_vec());
        assert_eq!(columns_for(&[]), vec![ListColumn::Name], "Name alone");
        assert_eq!(
            columns_for(&[Group, Type, Owner, Permissions, Modified, Size, Type]),
            vec![
                ListColumn::Name,
                ListColumn::Size,
                ListColumn::Modified,
                ListColumn::Type,
                ListColumn::Permissions,
                ListColumn::Owner,
                ListColumn::Group,
            ],
            "canonical order, no repeats"
        );
    }

    /// The optional columns read what the other views already show: the
    /// details panel's kind label and permission string, and the owner and
    /// group the scan resolved ("—" for an entry without them).
    #[test]
    fn type_permissions_owner_and_group_cells_read_the_entry() -> TestResult {
        let entries = vec![
            owned("photo.jpg", 0o640, "ana", "users"),
            entry("trash.txt", EntryKind::File),
        ];
        let mut source = EntryTableDelegate::new(
            WeakEntity::<Explorer>::new_invalid(),
            Arc::new(entries),
            Arc::new(vec![0, 1]),
        );
        source.set_columns(columns_for(&[
            ColumnPref::Type,
            ColumnPref::Permissions,
            ColumnPref::Owner,
            ColumnPref::Group,
        ]));
        assert_eq!(source.columns_count_for_tests(), 5);
        let text = |row, column| source.cell(row, column).map(|cell| cell.text);
        assert_eq!(text(0, 1).as_deref(), Some("Image \u{b7} JPG"));
        assert_eq!(text(0, 2).as_deref(), Some("rw-r-----"));
        assert_eq!(text(0, 3).as_deref(), Some("ana"));
        assert_eq!(text(0, 4).as_deref(), Some("users"));
        for column in 2..5 {
            assert_eq!(text(1, column).as_deref(), Some("\u{2014}"), "{column}");
        }
        assert_eq!(text(1, 1).as_deref(), Some("Document \u{b7} TXT"));
        let permissions = source.cell(0, 2).ok_or("exists")?;
        assert!(permissions.muted);
        assert_eq!(permissions.align, CellAlign::Left);
        assert_eq!(source.cell(0, 5), None, "no column past the count");
        Ok(())
    }

    /// Each header sorts by its own key, and the widths of the extra columns
    /// join the layout: Name still takes the surplus and the table still
    /// fills the pane.
    #[test]
    fn new_columns_sort_by_their_key_and_share_the_pane() {
        assert_eq!(ListColumn::Type.sorts_by(), SortKey::Type);
        assert_eq!(ListColumn::Permissions.sorts_by(), SortKey::Permissions);
        assert_eq!(ListColumn::Owner.sorts_by(), SortKey::Owner);
        assert_eq!(ListColumn::Group.sorts_by(), SortKey::Group);
        let all = columns_for(&[
            ColumnPref::Size,
            ColumnPref::Modified,
            ColumnPref::Type,
            ColumnPref::Permissions,
            ColumnPref::Owner,
            ColumnPref::Group,
        ]);
        let wide = distribute_widths(&all, Some(1_600.0), &HashMap::new());
        assert_eq!(wide.len(), 7);
        assert_eq!(wide.iter().sum::<f32>(), 1_600.0);
        let minima: f32 = all.iter().map(|column| column.min_width()).sum();
        let narrow = distribute_widths(&all, Some(minima - 1.0), &HashMap::new());
        assert_eq!(narrow[0], ListColumn::Name.min_width());
        assert!(
            narrow.iter().sum::<f32>() > minima - 1.0,
            "overflows sideways"
        );
        // A drag is remembered by column, wherever the column sits.
        let mut drags = HashMap::new();
        drags.insert(ListColumn::Owner, 200.0);
        assert_eq!(distribute_widths(&all, Some(1_600.0), &drags)[5], 200.0);
        let without_type = columns_for(&[ColumnPref::Owner]);
        assert_eq!(
            distribute_widths(&without_type, Some(900.0), &drags)[1],
            200.0
        );
    }

    /// The comparators for the new keys, through the reference order the
    /// index is proven equal to: permissions by number, owner and group by
    /// name without case, no data lowest, folders first, name as tiebreak.
    #[test]
    fn sorting_by_permissions_owner_and_group_is_stable_with_name_tiebreak() {
        let mut dir = owned("Zdir", 0o700, "zed", "wheel");
        dir.kind = EntryKind::Directory;
        let entries = vec![
            owned("b.txt", 0o644, "Ana", "users"),
            owned("a.txt", 0o644, "bob", "Users"),
            owned("c.txt", 0o600, "ana", "adm"),
            entry("none.txt", EntryKind::File),
            dir,
        ];
        let all: Vec<usize> = (0..entries.len()).collect();
        let order = |key, dir| names(&entries, &sorted_indices(&entries, &all, spec(key, dir)));
        assert_eq!(
            order(SortKey::Permissions, SortDir::Ascending),
            ["Zdir", "none.txt", "c.txt", "a.txt", "b.txt"],
            "folder first; no mode lowest; 600 < 644; equal modes by name"
        );
        assert_eq!(
            order(SortKey::Permissions, SortDir::Descending),
            ["Zdir", "a.txt", "b.txt", "c.txt", "none.txt"]
        );
        assert_eq!(
            order(SortKey::Owner, SortDir::Ascending),
            ["Zdir", "none.txt", "b.txt", "c.txt", "a.txt"],
            "ana (any case) before bob; the tie by name"
        );
        assert_eq!(
            order(SortKey::Group, SortDir::Ascending),
            ["Zdir", "none.txt", "c.txt", "a.txt", "b.txt"],
            "adm, then users/Users together (tie by name)"
        );
        assert_eq!(
            order(SortKey::Group, SortDir::Descending),
            ["Zdir", "a.txt", "b.txt", "c.txt", "none.txt"]
        );
    }
}
