//! The entries area: the measured container over the two virtualized
//! presentations — the folder tile grid and the details list. The list is
//! Bolt's own data grid (see [`super::table`]); the grid composes Bolt's
//! `ListRow`/icon/text primitives into fixed-size tiles, because Bolt has
//! no gallery primitive of its own.
//!
//! The grid derives its column count from the entries area's **measured**
//! inner width with a minimum of one, so no row renders the whole directory
//! and no keystroke rescans it. Tiles are fixed-width and never stretch:
//! a partial last row left-aligns exactly the remaining tiles. Clicks and
//! keyboard arrows go through the raw entry paths the state keeps; display
//! names are lossy and never used for navigation.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use bolt::composites::ListRow;
use bolt::primitives::Icon;
use bolt::tokens::Spacing;
use fileflux::core::{Entry, EntryKind, Location};
use gpui::{
    App, ClickEvent, Context, Div, ElementId, Hsla, InteractiveElement, IntoElement, Modifiers,
    ParentElement, Stateful, Styled, WeakEntity, Window, canvas, div, prelude::*, px,
};

use super::icon;
use super::state::View;
use super::view::{ENTRIES_CONTEXT, Explorer};

/// Tile width of the reference grid (normal density); every tile draws at
/// exactly this width, so a partial last row matches a full one.
const TILE_WIDTH: f32 = 104.0;
/// Tile top padding above the icon slot.
const TILE_TOP_PADDING: f32 = 12.0;
/// Height of the one icon slot every glyph draws in, so the folder
/// silhouette and the file glyphs share a baseline row.
const ICON_SLOT: f32 = 32.0;
/// Gap between the icon slot and the filename.
const NAME_GAP: f32 = 8.0;
/// One filename line's height; long names clip at two lines.
const NAME_LINE: f32 = 18.0;
/// Tile bottom padding under the filename.
const TILE_BOTTOM_PADDING: f32 = 8.0;
/// Tile height: top padding, the icon slot, the name gap, two clamped
/// filename lines and bottom padding — half of the earlier grid's vertical
/// dead space, with the filename at the primary 13 px text size.
const TILE_HEIGHT: f32 =
    TILE_TOP_PADDING + ICON_SLOT + NAME_GAP + 2.0 * NAME_LINE + TILE_BOTTOM_PADDING;
/// Gap between tiles.
const TILE_GAP: f32 = 8.0;
/// Horizontal padding of the entries area.
const ENTRIES_PADDING: f32 = 16.0;

/// The compact filled folder silhouette drawn in a tile's icon slot: the
/// bundled Lucide folder glyph is stroke-only, so the fill is composed
/// natively from a tab and a body of existing info blue — a silhouette, not
/// a square colored app-icon chip.
const FOLDER_WIDTH: f32 = 40.0;
/// Silhouette height; fits [`ICON_SLOT`].
const FOLDER_HEIGHT: f32 = 32.0;
/// Width of the raised folder tab.
const FOLDER_TAB_WIDTH: f32 = 16.0;
/// How far the tab rises above the body.
const FOLDER_TAB_HEIGHT: f32 = 14.0;
/// Where the body starts under the tab.
const FOLDER_BODY_TOP: f32 = 8.0;

/// Chooses the bundled Lucide glyph for a non-directory entry from the
/// file name's extension alone: no file contents are read and no MIME
/// probe runs. Extensions compare case-insensitively; anything the table
/// does not name — including extensionless names and dotfiles — stays the
/// plain file glyph.
pub(crate) fn file_glyph(name: &OsStr) -> gpui_kit_assets::IconName {
    use gpui_kit_assets::IconName;

    let extension = Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "tif" | "tiff" => {
            IconName::FileImage
        }
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "m4v" | "mpg" | "mpeg" => {
            IconName::FileVideoCamera
        }
        "mp3" | "flac" | "wav" | "ogg" | "oga" | "m4a" | "aac" | "opus" => IconName::FileMusic,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "iso" => {
            IconName::FileArchive
        }
        "csv" | "tsv" | "xls" | "xlsx" | "ods" => IconName::FileSpreadsheet,
        "rs" | "py" | "js" | "ts" | "jsx" | "tsx" | "html" | "css" | "json" | "toml" | "yaml"
        | "yml" | "xml" | "sh" | "c" | "h" | "cpp" | "hpp" | "go" | "java" | "kt" | "rb"
        | "php" | "lua" | "sql" => IconName::FileCode,
        "txt" | "md" | "log" | "conf" | "cfg" | "ini" | "rst" | "pdf" | "doc" | "docx" | "odt"
        | "rtf" => IconName::FileText,
        _ => IconName::File,
    }
}

/// The folder silhouette: one relative frame holding the filled tab and
/// the filled body, both in the given info blue. Two rectangles compose the
/// classic tab-and-body folder shape without any stroke or outline.
fn folder_silhouette(color: Hsla) -> impl IntoElement {
    div()
        .relative()
        .w(px(FOLDER_WIDTH))
        .h(px(FOLDER_HEIGHT))
        .child(
            div()
                .absolute()
                .left(px(0.0))
                .top(px(0.0))
                .w(px(FOLDER_TAB_WIDTH))
                .h(px(FOLDER_TAB_HEIGHT))
                .bg(color),
        )
        .child(
            div()
                .absolute()
                .left(px(0.0))
                .right(px(0.0))
                .top(px(FOLDER_BODY_TOP))
                .bottom(px(0.0))
                .bg(color),
        )
}

/// Derives the grid column count from the entries area's **inner** width —
/// the content box the tiles lay out in; the padding is already excluded by
/// the measurement — with a minimum of one so nothing is ever unreachable.
pub(crate) fn grid_columns(inner_width: f32) -> usize {
    let usable = inner_width.max(TILE_WIDTH);
    (((usable + TILE_GAP) / (TILE_WIDTH + TILE_GAP)).floor() as usize).max(1)
}

/// The tile indices of grid row `row`: a full row holds exactly `columns`
/// tiles, the last partial row holds exactly the remaining ones (never
/// stretched to fill the width), and a row past the end is empty.
pub(crate) fn grid_tile_range(row: usize, count: usize, columns: usize) -> std::ops::Range<usize> {
    let columns = columns.max(1);
    let start = (row * columns).min(count);
    start..(start + columns).min(count)
}

/// The entries area of the explorer: loading, empty, no-result and result
/// states. The result states live in one measured content wrapper: a
/// draw-nothing canvas records the wrapper's inner width, and the column
/// count follows that measurement instead of a viewport estimate.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let visible: Arc<Vec<usize>> = Arc::new(this.state.visible_indices());
    let count = visible.len();

    let container = div()
        .id("explorer-entries")
        .key_context(ENTRIES_CONTEXT)
        .track_focus(&this.entries_focus)
        .flex_1()
        .min_h_0()
        .min_w_0()
        .flex()
        .flex_col()
        .when(this.state.view() != View::List, |container| {
            container.px(px(ENTRIES_PADDING)).pb(Spacing::XL)
        });

    if this.state.is_loading() && count == 0 {
        return container.justify_center().child(
            div()
                .id("explorer-entries-loading")
                .text_color(colors.muted)
                .child("Loading…"),
        );
    }

    if this.state.folder_is_empty() {
        let empty_note = if this.state.current() == &Location::Trash {
            "Trash is empty. Deleted items would appear here."
        } else {
            "Favorites can point to an empty folder. Nothing to show yet."
        };
        return container.justify_center().child(
            div()
                .id("explorer-entries-empty")
                .flex()
                .flex_col()
                .items_center()
                .gap(Spacing::SM)
                .text_color(colors.muted)
                .child(
                    div()
                        .text_color(colors.strong)
                        .text_size(px(18.0))
                        .child("This folder is empty"),
                )
                .child(empty_note),
        );
    }

    if this.state.is_no_result() {
        // The folder has entries; the query filters everything out. This is
        // distinct from an empty folder.
        return container.justify_center().child(
            div()
                .id("explorer-entries-no-result")
                .flex()
                .flex_col()
                .items_center()
                .gap(Spacing::SM)
                .text_color(colors.muted)
                .child(
                    div()
                        .text_color(colors.strong)
                        .text_size(px(18.0))
                        .child(format!(
                            "No matches for \u{201c}{}\u{201d}",
                            this.state.query()
                        )),
                )
                .child("Search checks names in the current folder only, not its subfolders."),
        );
    }

    let weak = cx.entity().downgrade();
    let selected: Arc<BTreeSet<PathBuf>> = Arc::new(this.state.selection().clone());
    let list_view = this.state.view() == View::List;
    let columns = this.rendered_columns.max(1);
    let item_count = if list_view {
        count
    } else {
        count.div_ceil(columns)
    };
    let snapshot = Arc::clone(&this.snapshot);
    let weak_for_rows = weak.clone();
    let visible_for_rows = Arc::clone(&visible);
    let selected_for_rows = Arc::clone(&selected);

    // One vertical scroll owner per presentation: the two modes count rows
    // in different units (entries versus rows of tiles), so they keep
    // separate handles and separate list element ids.
    let rows = if list_view {
        super::table::render_list(this, visible, selected, &weak, cx).into_any_element()
    } else {
        gpui::uniform_list(
            "explorer-grid-rows",
            item_count,
            move |row_range, _window, cx: &mut App| {
                row_range
                    .map(|row| {
                        render_grid_row(
                            &snapshot,
                            &visible_for_rows,
                            row,
                            columns,
                            &selected_for_rows,
                            &weak_for_rows,
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&this.grid_scroll.clone())
        .into_any_element()
    };

    container.child(
        div()
            .id("explorer-entries-content")
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .child(measure_width(&weak))
            .child(rows),
    )
}

/// A draw-nothing canvas that records the entries content's measured inner
/// width in the explorer, so the grid columns and the keyboard row step use
/// the width the area actually has instead of a viewport-minus-sidebar
/// estimate. The width (and the columns derived from it) is stored only
/// when it changed, so the measurement never becomes a paint loop.
fn measure_width(weak: &WeakEntity<Explorer>) -> impl IntoElement {
    let weak = weak.clone();
    canvas(
        move |bounds, window, cx| {
            let width = f32::from(bounds.size.width);
            if let Err(error) = weak.update(cx, |this, cx| {
                if this.entries_width != Some(width) {
                    this.entries_width = Some(width);
                    cx.notify();
                    window.request_animation_frame();
                }
            }) {
                log::debug!("the entries area outlived its explorer: {error}");
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

/// One grid row: the tiles of [`grid_tile_range`], each fixed-width, on a
/// fixed-height left-aligned row.
fn render_grid_row(
    snapshot: &[Entry],
    visible: &[usize],
    row: usize,
    columns: usize,
    selected: &BTreeSet<PathBuf>,
    weak: &WeakEntity<Explorer>,
    cx: &mut App,
) -> gpui::AnyElement {
    let colors = bolt::colors(cx);
    let mut row_element = div()
        .id(ElementId::Name(format!("explorer-grid-row-{row}").into()))
        .flex()
        .gap(px(TILE_GAP))
        .h(px(TILE_HEIGHT))
        .min_w_0();
    for index in grid_tile_range(row, visible.len(), columns) {
        let Some(tile) = visible.get(index).and_then(|&i| snapshot.get(i)) else {
            continue;
        };
        let path = tile.path.clone();
        let name = tile.display_name().into_owned();
        let is_folder = tile.kind == EntryKind::Directory && !tile.is_symlink;
        let broken = tile.is_symlink && tile.kind == EntryKind::Symlink;
        let tile_selected = selected.contains(&path);

        // Folders draw the filled blue silhouette; a resolvable symlink
        // draws the symlink glyph; a broken one the warning glyph; every
        // other entry the extension-derived file glyph. Selection, hover
        // and clicks keep the raw path — the glyphs are display-only.
        let glyph = if broken {
            Icon::new(icon(gpui_kit_assets::IconName::Link2Off))
                .size(px(ICON_SLOT))
                .color(colors.warning)
                .into_any_element()
        } else if tile.is_symlink {
            Icon::new(icon(gpui_kit_assets::IconName::FileSymlink))
                .size(px(ICON_SLOT))
                .color(colors.muted)
                .into_any_element()
        } else if is_folder {
            folder_silhouette(colors.info).into_any_element()
        } else {
            Icon::new(icon(file_glyph(&tile.name)))
                .size(px(ICON_SLOT))
                .color(colors.muted)
                .into_any_element()
        };

        row_element = row_element.child(
            ListRow::new(ElementId::Name(
                format!("explorer-tile-{row}-{index}").into(),
            ))
            .selected(tile_selected)
            .selection_bar(true)
            .build(cx)
            .w(px(TILE_WIDTH))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(TILE_TOP_PADDING))
            .pb(px(TILE_BOTTOM_PADDING))
            .px(Spacing::XS)
            .on_click(entry_click(weak, path))
            .child(glyph)
            .child(
                div()
                    .mt(px(NAME_GAP))
                    .text_size(px(13.0))
                    .line_height(px(NAME_LINE))
                    .line_clamp(2)
                    .text_center()
                    .text_color(if tile_selected {
                        colors.strong
                    } else {
                        colors.foreground
                    })
                    .child(name),
            ),
        );
    }
    row_element.into_any_element()
}

/// The click handler of one entry: single click selects (Ctrl toggles),
/// double click opens. The selection uses raw paths; a click also focuses
/// the entries area, so the keyboard follows the pointer. Both the tiles
/// and the details grid's cells route through here.
pub(crate) fn handle_entry_click(
    weak: &WeakEntity<Explorer>,
    path: PathBuf,
    click_count: usize,
    control: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if let Err(error) = weak.update(cx, |this, cx| {
        this.entries_focus.focus(window, cx);
        if click_count >= 2 {
            this.open_path(&path, window, cx);
        } else if control {
            this.state.clear_notice();
            this.state.toggle(&path);
            cx.notify();
        } else {
            this.state.clear_notice();
            this.state.select_only(&path);
            cx.notify();
        }
    }) {
        log::debug!("the row outlived its explorer: {error}");
    }
}

/// The tile click handler: reads the click count and modifiers off the
/// event and applies [`handle_entry_click`].
fn entry_click(
    weak: &WeakEntity<Explorer>,
    path: PathBuf,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let weak = weak.clone();
    move |event: &ClickEvent, window, cx| {
        let click_count = event.click_count();
        let modifiers = match event {
            ClickEvent::Mouse(mouse) => mouse.down.modifiers,
            _ => Modifiers::default(),
        };
        handle_entry_click(
            &weak,
            path.clone(),
            click_count,
            modifiers.control,
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `grid_columns` answers with the column count of the entries area's
    /// **inner** (already padded) width: exact-width boundaries fit exactly
    /// that many tiles, one pixel less drops a column, and nothing ever
    /// collapses to zero columns, so the last row is always reachable.
    #[test]
    fn grid_columns_fits_exact_tile_boundaries_and_narrow_panes() {
        assert_eq!(grid_columns(TILE_WIDTH), 1);
        assert_eq!(grid_columns(0.0), 1, "never zero columns");
        assert_eq!(
            grid_columns(2.0 * TILE_WIDTH + TILE_GAP),
            2,
            "exactly two tiles fit"
        );
        assert_eq!(grid_columns(2.0 * TILE_WIDTH + TILE_GAP - 1.0), 1);
        assert_eq!(grid_columns(3.0 * TILE_WIDTH + 2.0 * TILE_GAP), 3);
        assert_eq!(grid_columns(3.0 * TILE_WIDTH + 2.0 * TILE_GAP - 1.0), 2);
        assert_eq!(grid_columns(4.0 * TILE_WIDTH + 3.0 * TILE_GAP), 4);
    }

    /// Every grid row, the last partial one included, addresses the same
    /// fixed tile geometry: a full row holds exactly `columns` tiles, the
    /// last partial row holds exactly the remaining tiles (never stretched
    /// to fill), and a row past the end is empty.
    #[test]
    fn grid_tile_rows_keep_full_and_partial_rows_identical() {
        assert_eq!(grid_tile_range(0, 10, 4), 0..4);
        assert_eq!(grid_tile_range(1, 10, 4), 4..8);
        assert_eq!(
            grid_tile_range(2, 10, 4),
            8..10,
            "the partial last row holds exactly the remaining tiles"
        );
        assert_eq!(grid_tile_range(3, 10, 4), 10..10, "past the end is empty");
        assert_eq!(
            grid_tile_range(1, 8, 4),
            4..8,
            "a divisible count has no partial row"
        );
        assert_eq!(grid_tile_range(0, 5, 0), 0..1, "no columns means one tile");
    }

    /// The extension-only glyph classifier maps common file types to
    /// distinct bundled Lucide glyphs from the file name alone: no file
    /// contents, no MIME probes, case-insensitive extensions, and the plain
    /// file glyph for everything it does not name.
    #[test]
    fn file_glyphs_distinguish_common_types_by_extension_alone() {
        use gpui_kit_assets::IconName;
        let glyph = |name: &str| file_glyph(std::ffi::OsStr::new(name));

        assert_eq!(glyph("notes.txt"), IconName::FileText);
        assert_eq!(glyph("README.md"), IconName::FileText);
        assert_eq!(glyph("report-draft.pdf"), IconName::FileText);
        assert_eq!(glyph("screenshot-session.PNG"), IconName::FileImage);
        assert_eq!(glyph("wallpaper-mountains.jpg"), IconName::FileImage);
        assert_eq!(glyph("demo-recording.mp4"), IconName::FileVideoCamera);
        assert_eq!(glyph("podcast-episode.mp3"), IconName::FileMusic);
        assert_eq!(glyph("backup-home.tar.gz"), IconName::FileArchive);
        assert_eq!(glyph("wallpaper-pack.zip"), IconName::FileArchive);
        assert_eq!(glyph("dataset-sample.csv"), IconName::FileSpreadsheet);
        assert_eq!(glyph("explorer.rs"), IconName::FileCode);
        assert_eq!(glyph("theme.css"), IconName::FileCode);
        assert_eq!(glyph("Cargo.toml"), IconName::FileCode);
        assert_eq!(glyph("budget-2026.ods"), IconName::FileSpreadsheet);
        // Nothing the classifier names stays a bare file glyph; everything
        // else does — including files without an extension and dotfiles.
        assert_eq!(glyph("Makefile"), IconName::File);
        assert_eq!(glyph("archive"), IconName::File);
        assert_eq!(glyph(".gitignore"), IconName::File, "dotfiles stay plain");
        assert_eq!(glyph("readme.txt.bak"), IconName::File);
    }

    /// The tile height is exactly its composition — top padding, one icon
    /// slot, the name gap and two clamped filename lines plus bottom
    /// padding — so the fixed row height can never silently grow away from
    /// the geometry the tiles draw, and the folder silhouette fits the same
    /// icon slot the file glyphs use.
    #[test]
    fn tile_height_matches_its_composition_and_slot() {
        let composed =
            TILE_TOP_PADDING + ICON_SLOT + NAME_GAP + 2.0 * NAME_LINE + TILE_BOTTOM_PADDING;
        assert_eq!(
            TILE_HEIGHT, composed,
            "the row height is the tile's own sum"
        );
        let (slot, folder_height, folder_width) = (ICON_SLOT, FOLDER_HEIGHT, FOLDER_WIDTH);
        assert!(folder_height <= slot, "silhouette fits the icon slot");
        assert!(folder_width <= TILE_WIDTH, "silhouette fits the tile width");
    }
}
