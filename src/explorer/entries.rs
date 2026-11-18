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

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use bolt::composites::ListRow;
use bolt::primitives::{Icon, WhenFocusVisible};
use bolt::tokens::{Borders, ChromeColors, Spacing};
use fileflux::core::places::format_bytes;
use fileflux::core::{Entry, EntryKind, Location};
use gpui::{
    App, ClickEvent, Context, Div, ElementId, Hsla, InteractiveElement, IntoElement, Modifiers,
    MouseButton, MouseDownEvent, ParentElement, Stateful, Styled, WeakEntity, Window, canvas, div,
    prelude::*, px,
};
use gpui_component::ActiveTheme;

use super::actions::RenameView;
use super::state::View;
use super::view::{ENTRIES_CONTEXT, Explorer};
use super::{icon, util};

/// Tile width of the reference grid (the artboard's 112 px minimum); every
/// tile draws at exactly this width, so a partial last row matches a full
/// one.
const TILE_WIDTH: f32 = 112.0;
/// Tile top padding above the thumbnail slot.
const TILE_TOP_PADDING: f32 = 12.0;
/// Width of the thumbnail slot every glyph, folder and photo draws in.
const THUMB_WIDTH: f32 = 64.0;
/// Height of the thumbnail slot, so the folder silhouette, the file glyphs
/// and the photo thumbnails share a baseline row.
const THUMB_HEIGHT: f32 = 52.0;
/// Size of a file glyph inside the thumbnail slot (thinner and larger than
/// the earlier 32 px glyph, as in the artboard).
const GLYPH_SIZE: f32 = 40.0;
/// Gap between the thumbnail slot and the filename.
const NAME_GAP: f32 = 8.0;
/// One filename line's height; long names clip at two lines.
const NAME_LINE: f32 = 17.0;
/// Gap between the filename and the size subline.
const SUB_GAP: f32 = 2.0;
/// The mono 11 px subline's height.
const SUB_LINE: f32 = 14.0;
/// Tile bottom padding under the subline.
const TILE_BOTTOM_PADDING: f32 = 4.0;
/// Tile height: top padding, the thumbnail slot, the name gap, two clamped
/// filename lines, the subline gap and subline, and bottom padding.
const TILE_HEIGHT: f32 = TILE_TOP_PADDING
    + THUMB_HEIGHT
    + NAME_GAP
    + 2.0 * NAME_LINE
    + SUB_GAP
    + SUB_LINE
    + TILE_BOTTOM_PADDING;
/// Gap between tiles.
const TILE_GAP: f32 = 6.0;
/// Opacity of an entry that was cut and waits for Paste.
pub(crate) const CUT_OPACITY: f32 = 0.45;
/// Alpha of the tint border around a selected tile (artboard `.35`).
const SELECTION_BORDER_ALPHA: f32 = 0.35;
/// Horizontal padding of the entries area.
const ENTRIES_PADDING: f32 = 16.0;
/// Grid top padding under the folder subbar (artboard 4 px).
const GRID_TOP_PADDING: f32 = 4.0;

/// The photo thumbnail box (artboard `.photo`): 62×46 with a 1 px border.
const PHOTO_WIDTH: f32 = 62.0;
const PHOTO_HEIGHT: f32 = 46.0;

/// The folder silhouette is composed from the artboard's 59×50 `folderArt`
/// viewBox, scaled so its height fills [`THUMB_HEIGHT`]; the bundled Lucide
/// folder glyph is stroke-only, so the two tones are composed natively.
const FOLDER_VIEW_WIDTH: f32 = 59.0;
const FOLDER_VIEW_HEIGHT: f32 = 50.0;

/// Whether a file gets a real thumbnail: decided from the extension alone,
/// never by reading the file. Only formats GPUI's image loader decodes
/// (heic and svg stay on the glyph).
pub(crate) fn has_thumbnail(name: &OsStr) -> bool {
    let extension = Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

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

/// One rectangle of the folder silhouette, given in artboard viewBox units
/// and drawn at `scale` pixels per unit.
fn folder_part(scale: f32, x: f32, y: f32, width: f32, height: f32, color: Hsla) -> Div {
    div()
        .absolute()
        .left(px(x * scale))
        .top(px(y * scale))
        .w(px(width * scale))
        .h(px(height * scale))
        .bg(color)
}

/// The two-tone folder of the artboard's `folderArt`, `height` pixels tall:
/// the back layer (tab plus back panel) in info at reduced opacity, the
/// front body in full info, and a faint lighter top edge in strong. The
/// grid draws it [`THUMB_HEIGHT`] tall, the details panel larger.
pub(crate) fn folder_silhouette(info: Hsla, strong: Hsla, height: f32) -> impl IntoElement {
    let back = info.opacity(0.55);
    let scale = height / FOLDER_VIEW_HEIGHT;
    div()
        .relative()
        .w(px(FOLDER_VIEW_WIDTH * scale))
        .h(px(height))
        .child(folder_part(scale, 2.0, 6.0, 20.0, 5.0, back))
        .child(folder_part(scale, 2.0, 11.0, 55.0, 4.0, back))
        .child(folder_part(scale, 2.0, 15.0, 55.0, 31.0, info))
        .child(folder_part(
            scale,
            2.0,
            15.0,
            55.0,
            3.0,
            strong.opacity(0.18),
        ))
}

/// A file glyph centered in the thumbnail slot.
fn file_glyph_element(name: gpui_kit_assets::IconName, color: Hsla) -> gpui::AnyElement {
    Icon::new(icon(name))
        .size(px(GLYPH_SIZE))
        .color(color)
        .into_any_element()
}

/// The real thumbnail of an image file: GPUI's image loader decodes the
/// path off the UI thread; while loading or on error the extension glyph
/// shows instead. Object-fit cover inside the 62×46 bordered box.
fn photo_thumbnail(path: PathBuf, name: &OsStr, colors: &bolt::colors::Colors) -> gpui::AnyElement {
    let glyph = file_glyph(name);
    let muted = colors.muted;
    div()
        .w(px(PHOTO_WIDTH))
        .h(px(PHOTO_HEIGHT))
        .border_1()
        .border_color(colors.line_2)
        .bg(colors.raised)
        .overflow_hidden()
        .child(
            gpui::img(path)
                .size_full()
                .object_fit(gpui::ObjectFit::Cover)
                .with_loading(move || centered_glyph(glyph, muted))
                .with_fallback(move || centered_glyph(glyph, muted)),
        )
        .into_any_element()
}

/// A glyph centered in the photo box, for the loading and error states.
fn centered_glyph(name: gpui_kit_assets::IconName, color: Hsla) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon(name)).size(px(28.0)).color(color))
        .into_any_element()
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
            container
                .px(px(ENTRIES_PADDING))
                .pt(px(GRID_TOP_PADDING))
                .pb(Spacing::XL)
        })
        // Right click on the empty space: the folder's menu. A click on an
        // entry stops here first (see `entry_context_menu`).
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.open_context_menu(event.position, None, window, cx);
            }),
        );

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
    let counts_for_rows = Arc::clone(&this.child_counts);
    let weak_for_rows = weak.clone();
    let visible_for_rows = Arc::clone(&visible);
    let selected_for_rows = Arc::clone(&selected);
    let cut: Arc<BTreeSet<PathBuf>> = Arc::new(this.clipboard.cut_paths());
    let renaming = this.renaming.as_ref().map(super::actions::Renaming::view);
    let renaming_for_rows = renaming.clone();

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
                        let data = GridData {
                            snapshot: &snapshot,
                            visible: &visible_for_rows,
                            selected: &selected_for_rows,
                            cut: &cut,
                            child_counts: &counts_for_rows,
                            renaming: renaming_for_rows.as_ref(),
                        };
                        render_grid_row(&data, row, columns, &weak_for_rows, cx)
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

/// What the grid rows read: the listing snapshot, the visible rows in
/// displayed order, the selection and the folder counts.
#[derive(Clone, Copy)]
struct GridData<'a> {
    snapshot: &'a [Entry],
    visible: &'a [usize],
    selected: &'a BTreeSet<PathBuf>,
    /// Entries cut and waiting for Paste, drawn dimmed.
    cut: &'a BTreeSet<PathBuf>,
    child_counts: &'a HashMap<PathBuf, Option<usize>>,
    /// The inline rename editor, drawn in place of its entry's name.
    renaming: Option<&'a RenameView>,
}

/// One grid row: the tiles of [`grid_tile_range`], each fixed-width, on a
/// fixed-height left-aligned row.
fn render_grid_row(
    data: &GridData<'_>,
    row: usize,
    columns: usize,
    weak: &WeakEntity<Explorer>,
    cx: &mut App,
) -> gpui::AnyElement {
    let GridData {
        snapshot,
        visible,
        selected,
        cut,
        child_counts,
        renaming,
    } = *data;
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
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
        let tile_cut = cut.contains(&path);

        // Folders draw the two-tone silhouette; a resolvable symlink the
        // symlink glyph; a broken one the warning glyph; image files a real
        // thumbnail; every other entry the extension-derived file glyph.
        // Selection, hover and clicks keep the raw path — the art is
        // display-only.
        let thumb = if broken {
            file_glyph_element(gpui_kit_assets::IconName::Link2Off, colors.warning)
        } else if tile.is_symlink {
            file_glyph_element(gpui_kit_assets::IconName::FileSymlink, colors.muted)
        } else if is_folder {
            folder_silhouette(colors.info, colors.strong, THUMB_HEIGHT).into_any_element()
        } else if has_thumbnail(&tile.name) {
            photo_thumbnail(path.clone(), &tile.name, &colors)
        } else {
            file_glyph_element(file_glyph(&tile.name), colors.muted)
        };
        // Folders show their item count once the background count arrived;
        // files their size.
        let subline = if is_folder {
            child_counts
                .get(&tile.path)
                .copied()
                .flatten()
                .map(util::format_item_count)
        } else {
            tile.size.map(format_bytes)
        };

        let mut tile_element = ListRow::new(ElementId::Name(
            format!("explorer-tile-{row}-{index}").into(),
        ))
        .selected(tile_selected)
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
        .border_1()
        .border_color(if tile_selected {
            tint.opacity(SELECTION_BORDER_ALPHA)
        } else {
            gpui::transparent_black()
        })
        .when(tile_cut, |tile| tile.opacity(CUT_OPACITY))
        .on_click(entry_click(weak, path.clone()))
        .on_mouse_down(MouseButton::Right, entry_context_menu(weak, path.clone()))
        .child(
            div()
                .w(px(THUMB_WIDTH))
                .h(px(THUMB_HEIGHT))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(thumb),
        )
        .child(match renaming.filter(|view| view.path == path) {
            Some(view) => div()
                .mt(px(NAME_GAP))
                .w_full()
                .child(super::overlay::renamer(view, weak, true, cx))
                .into_any_element(),
            None => div()
                .mt(px(NAME_GAP))
                .max_w_full()
                .text_size(px(13.0))
                .line_height(px(NAME_LINE))
                .line_clamp(2)
                .text_center()
                .text_color(if tile_selected {
                    colors.strong
                } else {
                    colors.foreground
                })
                .child(name)
                .into_any_element(),
        });
        if let Some(subline) = subline {
            tile_element = tile_element.child(
                div()
                    .mt(px(SUB_GAP))
                    .h(px(SUB_LINE))
                    .font_family(bolt::fonts::editor_family(cx))
                    .text_size(px(11.0))
                    .line_height(px(SUB_LINE))
                    .text_color(colors.muted)
                    .child(subline),
            );
        }
        if tile_selected {
            // Keyboard focus ring: a 1.5 px tint outline inside the tile,
            // shown only in keyboard modality and distinct from the washed,
            // 1 px bordered selection.
            tile_element = tile_element.child(WhenFocusVisible::new(
                div()
                    .absolute()
                    .inset_0()
                    .border(Borders::FOCUS_RING)
                    .border_color(tint),
            ));
        }
        row_element = row_element.child(tile_element);
    }
    row_element.into_any_element()
}

/// The click handler of one entry: single click selects (Ctrl toggles,
/// Shift extends a range in displayed order), double click opens. The selection uses raw paths; a click also focuses
/// the entries area, so the keyboard follows the pointer. Both the tiles
/// and the details grid's cells route through here.
pub(crate) fn handle_entry_click(
    weak: &WeakEntity<Explorer>,
    path: PathBuf,
    click_count: usize,
    modifiers: Modifiers,
    window: &mut Window,
    cx: &mut App,
) {
    if let Err(error) = weak.update(cx, |this, cx| {
        this.entries_focus.focus(window, cx);
        if click_count >= 2 {
            this.open_path(&path, window, cx);
        } else if modifiers.shift {
            this.state.clear_notice();
            this.state.select_range_to(&path);
            cx.notify();
        } else if modifiers.control {
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

/// The right-click handler of one entry: selects it (unless it is already
/// part of the selection) and opens the menu under the pointer. Stops the
/// press so the entries area behind it does not open the empty-space menu.
pub(crate) fn entry_context_menu(
    weak: &WeakEntity<Explorer>,
    path: PathBuf,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
    let weak = weak.clone();
    move |event: &MouseDownEvent, window, cx| {
        cx.stop_propagation();
        if let Err(error) = weak.update(cx, |this, cx| {
            this.entries_focus.focus(window, cx);
            this.open_context_menu(event.position, Some(&path), window, cx);
        }) {
            log::debug!("the row outlived its explorer: {error}");
        }
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
        handle_entry_click(&weak, path.clone(), click_count, modifiers, window, cx);
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

    /// The tile height is exactly its composition, the folder silhouette
    /// fits the thumbnail slot, and the photo box fits it too.
    #[test]
    fn tile_height_matches_its_composition_and_slot() {
        let composed = TILE_TOP_PADDING
            + THUMB_HEIGHT
            + NAME_GAP
            + 2.0 * NAME_LINE
            + SUB_GAP
            + SUB_LINE
            + TILE_BOTTOM_PADDING;
        assert_eq!(
            TILE_HEIGHT, composed,
            "the row height is the tile's own sum"
        );
        assert_eq!(TILE_WIDTH, 112.0);
        assert_eq!(THUMB_HEIGHT, 52.0);
        let folder_width = FOLDER_VIEW_WIDTH * THUMB_HEIGHT / FOLDER_VIEW_HEIGHT;
        let thumb_width = THUMB_WIDTH;
        assert!(
            folder_width <= thumb_width,
            "silhouette fits the slot width"
        );
        let tile_width = TILE_WIDTH;
        assert!(thumb_width <= tile_width, "slot fits the tile width");
        assert!((PHOTO_WIDTH, PHOTO_HEIGHT) <= (THUMB_WIDTH, THUMB_HEIGHT));
        let (glyph, thumb_height) = (GLYPH_SIZE, THUMB_HEIGHT);
        assert!(glyph <= thumb_height);
    }

    /// Thumbnails are chosen from the extension alone, case-insensitively,
    /// for decodable raster formats only.
    #[test]
    fn thumbnails_are_decided_by_extension_alone() {
        let eligible = |name: &str| has_thumbnail(std::ffi::OsStr::new(name));
        for name in ["a.png", "a.JPG", "a.jpeg", "a.gif", "a.webp", "a.bmp"] {
            assert!(eligible(name), "{name}");
        }
        for name in ["a.heic", "a.svg", "a.txt", "png", ".png", "a.png.bak", "a"] {
            assert!(!eligible(name), "{name}");
        }
    }
}
