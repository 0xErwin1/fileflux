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
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::SystemTime;

use bolt::composites::ListRow;
use bolt::primitives::{Icon, WhenFocusVisible};
use bolt::tokens::{Borders, ChromeColors, Spacing};
use fileflux::core::places::format_bytes;
use fileflux::core::thumbnails::Bucket;
use fileflux::core::zoom::{CompactZoom, GridZoom};
use fileflux::core::{Entry, EntryKind, Location};
use gpui::{
    App, ClickEvent, Context, Div, ElementId, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement, RenderImage, Stateful,
    Styled, WeakEntity, Window, canvas, div, prelude::*, px,
};
use gpui_component::ActiveTheme;
use image::{Frame, RgbaImage};

use super::actions::{MenuOn, RenameView};
use super::dnd::{self, DropTarget};
use super::panes::Side;
use super::state::View;
use super::view::{ENTRIES_CONTEXT, Explorer};
use super::zoom::{
    CompactZoomExt as _, GridMetrics, GridZoomExt as _, NAME_GAP, NAME_LINE, SUB_GAP, SUB_LINE,
    TILE_BOTTOM_PADDING, TILE_GAP, TILE_TOP_PADDING,
};
use super::{icon, util};

/// Opacity of an entry that was cut and waits for Paste.
pub(crate) const CUT_OPACITY: f32 = 0.45;
/// Alpha of the tint border around a selected tile (artboard `.35`).
const SELECTION_BORDER_ALPHA: f32 = 0.35;
/// Alpha of the tint outline around what an open context menu acts on:
/// clearly stronger than a plain selection's border.
pub(crate) const CONTEXT_OUTLINE_ALPHA: f32 = 0.9;
/// Alpha of the tint wash under that outline.
pub(crate) const CONTEXT_WASH_ALPHA: f32 = 0.14;
/// Alpha of the quieter inset border around the entries area while the
/// menu of its empty space is open.
const AREA_OUTLINE_ALPHA: f32 = 0.5;
/// Alpha of the strong-text wash a hovered, unselected entry draws. The
/// palette's own hover wash is 4 % and cannot be told from the island.
const HOVER_WASH_ALPHA: f32 = 0.08;

/// The wash under the pointer on an unselected entry: visible on the panel
/// fill yet distinct from the tinted selection.
pub(crate) fn hover_wash(colors: &bolt::colors::Colors) -> Hsla {
    colors.strong.opacity(HOVER_WASH_ALPHA)
}
/// Horizontal padding of the entries area.
pub(crate) const ENTRIES_PADDING: f32 = 16.0;
/// Grid top padding under the folder subbar (artboard 4 px).
const GRID_TOP_PADDING: f32 = 4.0;

/// The folder silhouette is composed from the artboard's 59×50 `folderArt`
/// viewBox, scaled so its height fills the level's thumbnail slot; the bundled Lucide
/// folder glyph is stroke-only, so the two tones are composed natively.
const FOLDER_VIEW_WIDTH: f32 = 59.0;
const FOLDER_VIEW_HEIGHT: f32 = 50.0;

/// Whether a file may get a real thumbnail: decided from the extension alone,
/// never by reading the file. The raster images the app decodes itself, plus
/// the types an installed freedesktop thumbnailer may cover — videos, PDFs,
/// office documents, vector images — so a tile reserves the slot and the
/// background job finds out whether there is a preview to put in it.
pub(crate) fn has_thumbnail(name: &OsStr) -> bool {
    fileflux::core::mimes::previewable(name)
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
/// grid draws it as tall as the zoom level's thumbnail slot, the details
/// panel larger.
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
fn file_glyph_element(name: gpui_kit_assets::IconName, color: Hsla, size: f32) -> gpui::AnyElement {
    Icon::new(icon(name))
        .size(px(size))
        .color(color)
        .into_any_element()
}

/// The icon of a file's type, `size` pixels square: the installed icon
/// theme's icon when it has one — Blender's for `.blend`, the script glyph
/// for a shell script, the document glyph for a PDF — rasterized in the
/// theme's own colours (see [`theme_icon`]); else the bundled Lucide glyph
/// [`file_glyph`] picks, drawn in `color`. Folders, symlinks and broken
/// links never come here: they keep their own art.
pub(crate) fn file_icon(name: &OsStr, color: Hsla, size: f32) -> gpui::AnyElement {
    theme_icon(fileflux::core::icons::for_file(name), size)
        .unwrap_or_else(|| file_glyph_element(file_glyph(name), color, size))
}

/// Every theme icon drawn so far, rasterized once: keyed by the icon's
/// file and its box in whole pixels, so each (icon, size) is rasterized on
/// its first frame only — never again per frame per tile — and a raster
/// that failed once (`None`) is not retried either.
static RASTERS: LazyLock<Mutex<Rasters>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The rasterized theme icons: the icon file and its box in whole pixels
/// to the drawn image (`None` when rasterizing that icon failed).
type Rasters = HashMap<(PathBuf, u32), Option<Arc<RenderImage>>>;

/// The theme icon at `path` rasterized in colour into `size`-pixel square
/// pixels, memoized per (path, size) for the whole process (see
/// [`RASTERS`]). GPUI's image pipeline draws BGRA, so the raster's straight
/// RGBA is channel-swapped on the way in — the same swap GPUI's own image
/// loader applies to decoded files. `None` when the icon cannot be
/// rasterized.
fn rasterized(path: &Path, size: u32) -> Option<Arc<RenderImage>> {
    let mut rasters = RASTERS.lock().unwrap_or_else(PoisonError::into_inner);
    rasters
        .entry((path.to_owned(), size))
        .or_insert_with(|| {
            let rendered = fileflux::core::icons::render(path, size)?;
            let mut pixels = RgbaImage::from_raw(rendered.width, rendered.height, rendered.rgba)?;
            for pixel in pixels.as_chunks_mut::<4>().0 {
                // Straight RGBA to straight BGRA, the byte order GPUI draws.
                pixel.swap(0, 2);
            }
            Some(Arc::new(RenderImage::new([Frame::new(pixels)])))
        })
        .clone()
}

/// A theme icon file drawn `size` pixels square: rasterized in the theme's
/// own colours (see [`rasterized`]) and drawn as an image, because GPUI's
/// `svg` element paints an alpha mask — one single-colour silhouette,
/// which is exactly what a themed icon must not be. `None` without a file
/// or without a rasterizable one, so the caller keeps its Lucide glyph.
pub(crate) fn theme_icon(found: Option<PathBuf>, size: f32) -> Option<gpui::AnyElement> {
    let path = found?;
    // Whole pixels for the raster and the memo key, so a fractional box
    // never rasterizes the same icon twice as two sizes.
    let pixels = size.round().max(1.0) as u32;
    let image = rasterized(&path, pixels)?;
    Some(gpui::img(image).w(px(size)).h(px(size)).into_any_element())
}

/// What the background lookup found for one image: its thumbnail (cached or
/// generated), or `None` when there is none and the glyph stays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Thumb {
    /// The entry's modification time when it was looked up.
    pub(crate) modified: Option<SystemTime>,
    pub(crate) found: Option<PathBuf>,
    /// How many lookups of this unchanged file found nothing (see
    /// [`misses_after`]); `0` when a thumbnail was found.
    pub(crate) misses: u32,
}

/// A file with no thumbnail is looked up this many times while it stays
/// unchanged (the first answer plus one retry, for a thumbnailer installed
/// or a cache written since); after that it keeps its glyph until it changes.
const MAX_MISSES: u32 = 2;

/// The miss count of a lookup that found nothing for a file modified at
/// `modified`, given the answer `previous` it replaces: the misses add up
/// while the file is unchanged and start over when it changed.
pub(crate) fn misses_after(previous: Option<&Thumb>, modified: Option<SystemTime>) -> u32 {
    match previous {
        Some(thumb) if thumb.modified == modified && thumb.found.is_none() => {
            thumb.misses.saturating_add(1)
        }
        _ => 1,
    }
}

/// The lookups of one listing, by image path. A tile with no answer yet
/// shows its glyph: rendering never touches the disk to find one.
pub(crate) type Thumbs = HashMap<PathBuf, Thumb>;

/// Whether the cached thumbnail `found` fills a box that needs `wanted`.
/// A file outside the cache's size directories (an image that is itself in
/// the cache) is its own thumbnail and always does.
fn large_enough(found: &Path, wanted: Bucket) -> bool {
    Bucket::of_thumbnail(found).is_none_or(|bucket| bucket >= wanted)
}

/// An image whose thumbnail is looked up: a regular file (a symlink shows
/// the symlink glyph) of a format GPUI decodes.
fn wants_thumbnail(entry: &Entry) -> bool {
    entry.kind == EntryKind::File && !entry.is_symlink && has_thumbnail(&entry.name)
}

/// What a new listing does with the lookups of the previous one: the answers
/// that still apply stay on screen (`.0`) while the images that need a
/// lookup (`.1`) are looked up again, the displayed images first: `displayed`
/// holds the indices into `snapshot` in the order the view shows them, so
/// the tiles on screen resolve first; the images the view hides follow in
/// listing order, and an index outside the snapshot is skipped. With `trust_known`
/// an image whose entry time did not change keeps its answer; the Recent
/// list shows when a file was *used*, which says nothing about the file
/// changing, so it trusts none. A found thumbnail from a size directory
/// smaller than `wanted` (the zoom grew past what it can fill) stays on
/// screen and is looked up again in the bigger size; one that is big enough
/// is never looked up again for a zoom change alone.
pub(crate) fn plan_thumbnails(
    snapshot: &[Entry],
    displayed: &[usize],
    known: &Thumbs,
    trust_known: bool,
    wanted: Bucket,
) -> (Thumbs, Vec<(PathBuf, Option<SystemTime>)>) {
    let mut keep = Thumbs::new();
    let mut todo = Vec::new();
    let mut seen = vec![false; snapshot.len()];
    let mut order = Vec::with_capacity(snapshot.len());
    for &index in displayed {
        if let Some(seen) = seen.get_mut(index)
            && !std::mem::replace(seen, true)
        {
            order.push(index);
        }
    }
    order.extend(
        seen.iter()
            .enumerate()
            .filter_map(|(index, seen)| (!seen).then_some(index)),
    );
    let ordered = order.into_iter().filter_map(|index| snapshot.get(index));
    for entry in ordered.filter(|entry| wants_thumbnail(entry)) {
        match known.get(&entry.path) {
            // A thumbnail that was found stays while the file is unchanged. A
            // file that had none is asked once more, because the reason may
            // have gone away (a thumbnailer installed, its cache written by
            // another application); a second miss on the same file settles it
            // until the file changes, so a file that can never be thumbnailed
            // does not spawn the thumbnailer on every listing.
            Some(thumb)
                if trust_known
                    && thumb.modified == entry.modified
                    && match &thumb.found {
                        Some(found) => large_enough(found, wanted),
                        None => thumb.misses >= MAX_MISSES,
                    } =>
            {
                keep.insert(entry.path.clone(), thumb.clone());
            }
            Some(thumb) => {
                keep.insert(entry.path.clone(), thumb.clone());
                todo.push((entry.path.clone(), entry.modified));
            }
            None => todo.push((entry.path.clone(), entry.modified)),
        }
    }
    (keep, todo)
}

/// The real thumbnail of an image file, once its lookup has answered (see
/// [`Thumbs`]): GPUI's image loader decodes the small freedesktop thumbnail
/// (found in the cache or generated by Fileflux) off the UI thread; while it
/// loads or on error the type's icon shows instead — the installed theme's
/// when it has one, else the extension glyph. The full image is never
/// decoded for a tile: with no thumbnail (not yet made, too large, on a
/// remote mount, undecodable) that icon shows alone. Object-fit cover
/// inside the level's bordered photo box (62×46 at Medium).
fn photo_thumbnail(
    thumb: Option<&Thumb>,
    name: &OsStr,
    colors: &bolt::colors::Colors,
    metrics: &GridMetrics,
) -> gpui::AnyElement {
    let glyph = file_glyph(name);
    let theme = fileflux::core::icons::for_file(name);
    let Some(path) = thumb.and_then(|thumb| thumb.found.clone()) else {
        return div()
            .w(px(metrics.photo_width))
            .h(px(metrics.photo_height))
            .border_1()
            .border_color(colors.line_2)
            .bg(colors.raised)
            .overflow_hidden()
            .child(centered_art(
                theme,
                glyph,
                colors.muted,
                metrics.photo_glyph,
            ))
            .into_any_element();
    };
    let muted = colors.muted;
    let photo_glyph = metrics.photo_glyph;
    let loading_theme = theme.clone();
    div()
        .w(px(metrics.photo_width))
        .h(px(metrics.photo_height))
        .border_1()
        .border_color(colors.line_2)
        .bg(colors.raised)
        .overflow_hidden()
        .child(
            gpui::img(path)
                .size_full()
                .object_fit(gpui::ObjectFit::Cover)
                .with_loading(move || {
                    centered_art(loading_theme.clone(), glyph, muted, photo_glyph)
                })
                .with_fallback(move || centered_art(theme.clone(), glyph, muted, photo_glyph)),
        )
        .into_any_element()
}

/// What the photo box draws when the thumbnail is not there — not found,
/// still loading or failed to decode: the theme's type icon when it has
/// one, else the bundled Lucide glyph, centered.
fn centered_art(
    theme: Option<PathBuf>,
    glyph: gpui_kit_assets::IconName,
    color: Hsla,
    size: f32,
) -> gpui::AnyElement {
    match theme_icon(theme, size) {
        Some(art) => div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(art)
            .into_any_element(),
        None => centered_glyph(glyph, color, size),
    }
}

/// A glyph centered in the photo box, for the loading and error states.
fn centered_glyph(name: gpui_kit_assets::IconName, color: Hsla, size: f32) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon(name)).size(px(size)).color(color))
        .into_any_element()
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
    let visible: Arc<Vec<usize>> = this.tab().state.visible_shared();
    let count = visible.len();
    // The empty space's menu is open: the area shows it acts on the folder.
    let empty_space_menu = this
        .menu
        .as_ref()
        .is_some_and(|menu| menu.on == MenuOn::EmptySpace);

    let container = div()
        .id("explorer-entries")
        .key_context(ENTRIES_CONTEXT)
        .track_focus(this.entries_focus())
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if this.type_ahead(event, window, cx) {
                cx.stop_propagation();
            }
        }))
        .flex_1()
        .min_h_0()
        .min_w_0()
        .flex()
        .flex_col()
        .when(this.tab().state.view() != View::List, |container| {
            container
                .px(px(ENTRIES_PADDING))
                .pt(px(GRID_TOP_PADDING))
                .pb(Spacing::XL)
        })
        // A left press anywhere in the pane, margins included, may start a
        // rubber band (see `rubber::classify`).
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.rubber_press(event, window, cx);
            }),
        )
        // Right click on the empty space: the folder's menu. A click on an
        // entry stops here first (see `entry_context_menu`).
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.open_context_menu(event.position, None, window, cx);
            }),
        )
        // The 1 px border is transparent until a drop is hovering: the
        // highlight then never moves the layout.
        .border_1()
        .border_color(if empty_space_menu {
            ChromeColors::tint(cx.theme()).opacity(AREA_OUTLINE_ALPHA)
        } else {
            gpui::transparent_black()
        })
        .when(empty_space_menu, |container| {
            container.bg(ChromeColors::tint(cx.theme()).opacity(0.04))
        });
    // The empty space of a shown folder takes drops into that folder (an
    // entry that is no target covers it while dragging, see
    // `dnd::block_while_dragging`). The Trash and Recent views take none.
    let weak_for_drops = cx.entity().downgrade();
    let container = match this.tab().state.current() {
        Location::Directory(dir) => dnd::drop_target(
            container,
            DropTarget::Folder(dir.clone()),
            &weak_for_drops,
            cx,
        ),
        Location::Trash | Location::Recent | Location::Search { .. } | Location::Usage { .. } => {
            container
        }
    };

    if let Location::Usage { .. } = this.tab().state.current() {
        return super::usage_view::render(this, cx);
    }

    if this.tab().state.is_loading() && count == 0 {
        return container.justify_center().child(
            div()
                .id("explorer-entries-loading")
                .text_color(colors.muted)
                .child("Loading…"),
        );
    }

    if let Some(query) = this.tab().state.search_query()
        && this.tab().state.folder_is_empty()
    {
        return container.justify_center().child(search_empty_state(
            query,
            this.tab().state.search_status(),
            &colors,
        ));
    }

    if this.tab().state.folder_is_empty() {
        let empty_note = if this.tab().state.current() == &Location::Trash {
            "Trash is empty. Deleted items would appear here."
        } else if this.tab().state.current() == &Location::Recent {
            "No recent files. Files you open in other applications would appear here."
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

    if this.tab().state.is_no_result() {
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
                .child(div().text_color(colors.strong).text_size(px(18.0)).child(
                    if this.tab().state.query().is_empty() {
                        "Nothing matches the filters".to_owned()
                    } else {
                        format!(
                            "No matches for \u{201c}{}\u{201d}",
                            this.tab().state.query()
                        )
                    },
                ))
                .child(if this.tab().state.query().is_empty() {
                    "Remove a filter chip, or use Filter \u{203a} Clear all filters."
                } else {
                    "Search and filters check names in the current folder only, not its subfolders."
                }),
        );
    }

    let weak = cx.entity().downgrade();
    let selected: Arc<BTreeSet<PathBuf>> = Arc::new(this.tab().state.selection().clone());
    let view = this.tab().state.view();
    let list_view = view == View::List;
    let compact_view = view == View::Compact;
    let in_recent = this.tab().state.current().lists_files_from_elsewhere();
    let columns = this.rendered_columns().max(1);
    let zoom = this.grid_zoom;
    let compact_zoom = this.compact_zoom;
    let item_count = if list_view {
        count
    } else {
        count.div_ceil(columns)
    };
    let snapshot = Arc::clone(&this.tab().snapshot);
    let counts_for_rows = Arc::clone(this.tab().state.child_counts());
    let thumbs_for_rows = Arc::clone(&this.tab().thumbs);
    let weak_for_rows = weak.clone();
    let visible_for_rows = Arc::clone(&visible);
    let selected_for_rows = Arc::clone(&selected);
    let cut: Arc<BTreeSet<PathBuf>> = Arc::new(this.clipboard.cut_paths());
    let renaming = this.renaming.as_ref().map(super::actions::Renaming::view);
    let renaming_for_rows = renaming.clone();
    // Entries drag from the folder views, never from the Trash view.
    let drag: Option<Arc<Vec<PathBuf>>> = (!this.in_trash()).then(|| {
        Arc::new(
            this.tab()
                .state
                .visible_selected()
                .into_iter()
                .map(|entry| entry.path.clone())
                .collect(),
        )
    });
    let drag_for_rows = drag.clone();
    // The selection is what the open entry menu acts on.
    let outlined = this
        .menu
        .as_ref()
        .is_some_and(|menu| menu.on == MenuOn::Entries);

    // One vertical scroll owner per presentation: the two modes count rows
    // in different units (entries versus rows of tiles), so they keep
    // separate handles and separate list element ids.
    let rows = if list_view {
        super::table::render_list(this, visible, selected, drag, outlined, &weak, cx)
            .into_any_element()
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
                            thumbs: &thumbs_for_rows,
                            renaming: renaming_for_rows.as_ref(),
                            drag: drag_for_rows.as_ref(),
                            outlined,
                            in_recent,
                            zoom,
                        };
                        if compact_view {
                            render_compact_row(
                                &data,
                                compact_zoom,
                                row,
                                columns,
                                &weak_for_rows,
                                cx,
                            )
                        } else {
                            render_grid_row(&data, row, columns, &weak_for_rows, cx)
                        }
                    })
                    .collect::<Vec<_>>()
            },
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&this.tab().grid_scroll.clone())
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
            .child(measure_width(&weak, this.tabs.focus()))
            .child(rows)
            .children(this.rubber_overlay(this.tabs.focus(), cx)),
    )
}

/// A draw-nothing canvas that records the entries content's measured inner
/// width in the explorer, so the grid columns and the keyboard row step use
/// the width the area actually has instead of a viewport-minus-sidebar
/// estimate. The width (and the columns derived from it) is stored only
/// when it changed, so the measurement never becomes a paint loop.
fn measure_width(weak: &WeakEntity<Explorer>, side: Side) -> impl IntoElement {
    let weak = weak.clone();
    let listener = weak.clone();
    canvas(
        move |bounds, window, cx| {
            let width = f32::from(bounds.size.width);
            if let Err(error) = weak.update(cx, |this, cx| {
                this.record_entries_bounds(side, bounds);
                if this.record_entries_width(side, width) {
                    cx.notify();
                    window.request_animation_frame();
                }
            }) {
                log::debug!("the entries area outlived its explorer: {error}");
            }
        },
        move |_, (), window, _| super::rubber::listen(&listener, side, window),
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
    /// The background thumbnail lookups.
    thumbs: &'a Thumbs,
    /// The inline rename editor, drawn in place of its entry's name.
    renaming: Option<&'a RenameView>,
    /// The visible selection, when tiles may start a drag (not in the Trash).
    drag: Option<&'a Arc<Vec<PathBuf>>>,
    /// The selection is the target of an open context menu.
    outlined: bool,
    /// The Recent view and search results: a tile's second line names the
    /// folder the file is in.
    in_recent: bool,
    /// The grid's icon size level.
    zoom: GridZoom,
}

/// The message of a search with nothing to list: still looking, nothing
/// found, or no query yet.
fn search_empty_state(
    query: &str,
    status: &super::state::SearchStatus,
    colors: &bolt::colors::Colors,
) -> gpui::Stateful<gpui::Div> {
    let (title, note) = if query.is_empty() {
        (
            "Type to search subfolders".to_owned(),
            "Names are matched in this folder and everything under it.",
        )
    } else if status.searching {
        (
            "Searching\u{2026}".to_owned(),
            "Matches appear here as they are found. Escape stops the search.",
        )
    } else if status.stopped {
        (
            "Search stopped".to_owned(),
            "Nothing was found before it stopped.",
        )
    } else {
        (
            format!("No files named \u{201c}{query}\u{201d}"),
            "The search covers this folder and its subfolders, skips hidden files while they are hidden and never follows links or other drives.",
        )
    };
    div()
        .id("explorer-entries-search-empty")
        .flex()
        .flex_col()
        .items_center()
        .gap(Spacing::SM)
        .text_color(colors.muted)
        .child(
            div()
                .text_color(colors.strong)
                .text_size(px(18.0))
                .child(title),
        )
        .child(note)
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
        thumbs,
        renaming,
        drag,
        outlined,
        in_recent,
        zoom,
    } = *data;
    let metrics = zoom.metrics();
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let mut row_element = div()
        .id(ElementId::Name(format!("explorer-grid-row-{row}").into()))
        .flex()
        .gap(px(TILE_GAP))
        .h(px(metrics.tile_height()))
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
        // thumbnail; every other entry the installed theme's icon for its
        // type, or the extension-derived file glyph when the theme has
        // none. Selection, hover and clicks keep the raw path — the art is
        // display-only.
        let thumb = if broken {
            file_glyph_element(
                gpui_kit_assets::IconName::Link2Off,
                colors.warning,
                metrics.glyph,
            )
        } else if tile.is_symlink {
            file_glyph_element(
                gpui_kit_assets::IconName::FileSymlink,
                colors.muted,
                metrics.glyph,
            )
        } else if is_folder {
            folder_silhouette(colors.info, colors.strong, metrics.thumb_height).into_any_element()
        } else if has_thumbnail(&tile.name) {
            photo_thumbnail(thumbs.get(&path), &tile.name, &colors, &metrics)
        } else {
            file_icon(&tile.name, colors.muted, metrics.glyph)
        };
        // Folders show their item count once the background count arrived;
        // files their size.
        let subline = if is_folder {
            child_counts
                .get(&tile.path)
                .copied()
                .flatten()
                .map(util::format_item_count)
        } else if in_recent {
            // Where the file lives; its size is in the details panel.
            tile.origin.as_deref().map(|folder| {
                folder.file_name().map_or_else(
                    || folder.to_string_lossy().into_owned(),
                    |name| name.to_string_lossy().into_owned(),
                )
            })
        } else {
            tile.size.map(format_bytes)
        };

        let mut tile_element = ListRow::new(ElementId::Name(
            format!("explorer-tile-{row}-{index}").into(),
        ))
        .selected(tile_selected)
        // Bolt's own hover wash is 4 %: the tile draws a visible one below.
        .interactive(false)
        .build(cx)
        .cursor_pointer()
        .w(px(metrics.tile_width))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(TILE_TOP_PADDING))
        .pb(px(TILE_BOTTOM_PADDING))
        .px(Spacing::XS)
        .border_1()
        .border_color(if tile_selected && outlined {
            tint.opacity(CONTEXT_OUTLINE_ALPHA)
        } else if tile_selected {
            tint.opacity(SELECTION_BORDER_ALPHA)
        } else {
            gpui::transparent_black()
        })
        .when(tile_selected && outlined, |tile| {
            tile.bg(tint.opacity(CONTEXT_WASH_ALPHA))
        })
        .when(!tile_selected, |tile| {
            tile.hover(|tile| tile.bg(hover_wash(&colors)).border_color(colors.line_2))
        })
        .when(tile_cut, |tile| tile.opacity(CUT_OPACITY))
        .on_click(entry_click(weak, path.clone()))
        .on_mouse_up(MouseButton::Middle, entry_middle_click(weak, path.clone()))
        .on_mouse_down(MouseButton::Right, entry_context_menu(weak, path.clone()));
        let renaming_this = renaming.is_some_and(|view| view.path == path);
        if let Some(selection) = drag.filter(|_| !renaming_this) {
            let glyph = if is_folder {
                gpui_kit_assets::IconName::Folder
            } else {
                file_glyph(&tile.name)
            };
            tile_element = dnd::drag_source(
                tile_element,
                path.clone(),
                glyph,
                is_folder,
                tile_selected,
                selection,
                weak,
            );
        }
        // A real folder takes drops; any other tile only hides what is
        // behind it while dragging.
        tile_element = if is_folder && drag.is_some() {
            dnd::drop_target(tile_element, DropTarget::Folder(path.clone()), weak, cx)
        } else {
            tile_element
        };
        tile_element = dnd::block_while_dragging(tile_element, cx);
        // Named for assistive technology and for the automation bridge: the
        // tile is one entry, its label is what the user sees under the art,
        // and selection is exposed as the selected state.
        tile_element = tile_element
            .role(gpui::accesskit::Role::ListItem)
            .aria_label(name.clone())
            .aria_selected(tile_selected);
        let mut tile_element = tile_element
            .child(
                div()
                    .w(px(metrics.thumb_width))
                    .h(px(metrics.thumb_height))
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

/// One compact row: [`grid_tile_range`] cells of the compact view, each a
/// fixed-width cell with the entry's icon and a single-line name cut with an
/// ellipsis. No thumbnails, no second line: icons only.
fn render_compact_row(
    data: &GridData<'_>,
    zoom: CompactZoom,
    row: usize,
    columns: usize,
    weak: &WeakEntity<Explorer>,
    cx: &mut App,
) -> gpui::AnyElement {
    let metrics = zoom.metrics();
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let mut row_element = div()
        .id(ElementId::Name(
            format!("explorer-compact-row-{row}").into(),
        ))
        .flex()
        .gap(px(TILE_GAP))
        .h(px(metrics.cell_height))
        .min_w_0();
    for index in grid_tile_range(row, data.visible.len(), columns) {
        let Some(entry) = data.visible.get(index).and_then(|&i| data.snapshot.get(i)) else {
            continue;
        };
        let path = entry.path.clone();
        let name = entry.display_name().into_owned();
        let is_folder = entry.kind == EntryKind::Directory && !entry.is_symlink;
        let cell_selected = data.selected.contains(&path);
        let glyph = super::table::entry_glyph(entry);
        let color = match glyph {
            super::table::Glyph::Folder => colors.info,
            super::table::Glyph::BrokenLink => colors.warning,
            super::table::Glyph::File | super::table::Glyph::Symlink => colors.muted,
        };
        let icon_element = match glyph {
            super::table::Glyph::File => file_icon(&entry.name, color, metrics.icon),
            glyph => file_glyph_element(glyph.icon_name(), color, metrics.icon),
        };
        let mut cell = ListRow::new(ElementId::Name(
            format!("explorer-compact-{row}-{index}").into(),
        ))
        .selected(cell_selected)
        .interactive(false)
        .build(cx)
        .cursor_pointer()
        .w(px(metrics.cell_width))
        .flex_none()
        .h_full()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(Spacing::XS)
        .border_1()
        .border_color(if cell_selected && data.outlined {
            tint.opacity(CONTEXT_OUTLINE_ALPHA)
        } else if cell_selected {
            tint.opacity(SELECTION_BORDER_ALPHA)
        } else {
            gpui::transparent_black()
        })
        .when(cell_selected && data.outlined, |cell| {
            cell.bg(tint.opacity(CONTEXT_WASH_ALPHA))
        })
        .when(!cell_selected, |cell| {
            cell.hover(|cell| cell.bg(hover_wash(&colors)).border_color(colors.line_2))
        })
        .when(data.cut.contains(&path), |cell| cell.opacity(CUT_OPACITY))
        .on_click(entry_click(weak, path.clone()))
        .on_mouse_up(MouseButton::Middle, entry_middle_click(weak, path.clone()))
        .on_mouse_down(MouseButton::Right, entry_context_menu(weak, path.clone()));
        let renaming_this = data.renaming.is_some_and(|view| view.path == path);
        if let Some(selection) = data.drag.filter(|_| !renaming_this) {
            let drag_glyph = if is_folder {
                gpui_kit_assets::IconName::Folder
            } else {
                file_glyph(&entry.name)
            };
            cell = dnd::drag_source(
                cell,
                path.clone(),
                drag_glyph,
                is_folder,
                cell_selected,
                selection,
                weak,
            );
        }
        cell = if is_folder && data.drag.is_some() {
            dnd::drop_target(cell, DropTarget::Folder(path.clone()), weak, cx)
        } else {
            cell
        };
        cell = dnd::block_while_dragging(cell, cx);
        let cell = cell
            .role(gpui::accesskit::Role::ListItem)
            .aria_label(name.clone())
            .aria_selected(cell_selected)
            .child(div().flex_none().child(icon_element))
            .child(match data.renaming.filter(|view| view.path == path) {
                Some(view) => div()
                    .flex_1()
                    .min_w_0()
                    .child(super::overlay::renamer(view, weak, false, cx))
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .text_color(if cell_selected {
                        colors.strong
                    } else {
                        colors.foreground
                    })
                    .child(name)
                    .into_any_element(),
            });
        let cell = if cell_selected {
            cell.child(WhenFocusVisible::new(
                div()
                    .absolute()
                    .inset_0()
                    .border(Borders::FOCUS_RING)
                    .border_color(tint),
            ))
        } else {
            cell
        };
        row_element = row_element.child(cell);
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
        this.entries_focus().focus(window, cx);
        if click_count >= 2 {
            this.open_path(&path, window, cx);
        } else if modifiers.shift {
            this.tab_mut().state.clear_notice();
            this.tab_mut().state.select_range_to(&path);
            cx.notify();
        } else if modifiers.control {
            this.tab_mut().state.clear_notice();
            this.tab_mut().state.toggle(&path);
            cx.notify();
        } else {
            this.tab_mut().state.clear_notice();
            this.tab_mut().state.select_only(&path);
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
            this.entries_focus().focus(window, cx);
            this.open_context_menu(event.position, Some(&path), window, cx);
        }) {
            log::debug!("the row outlived its explorer: {error}");
        }
    }
}

/// The middle-click handler of one entry: a folder opens in a new
/// background tab; anything else does nothing.
pub(crate) fn entry_middle_click(
    weak: &WeakEntity<Explorer>,
    path: PathBuf,
) -> impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static {
    let weak = weak.clone();
    move |_event: &MouseUpEvent, window, cx| {
        cx.stop_propagation();
        if let Err(error) = weak.update(cx, |this, cx| {
            this.open_path_in_new_tab(&path, window, cx);
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

    /// Thumbnails are chosen from the extension alone, case-insensitively,
    /// for decodable raster formats only.
    #[test]
    fn thumbnails_are_decided_by_extension_alone() {
        let eligible = |name: &str| has_thumbnail(std::ffi::OsStr::new(name));
        for name in [
            "a.png", "a.JPG", "a.jpeg", "a.gif", "a.webp", "a.bmp", "a.mp4", "a.MKV", "a.pdf",
            "a.odt", "a.svg", "a.heic",
        ] {
            assert!(eligible(name), "{name}");
        }
        // Code, archives and extensionless names keep their glyph: no
        // thumbnailer claims them.
        for name in ["a.txt", "a.rs", "a.zip", "png", ".png", "a.png.bak", "a"] {
            assert!(!eligible(name), "{name}");
        }
    }

    fn image(name: &str, secs: u64) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from("/p").join(name),
            kind: EntryKind::File,
            is_symlink: false,
            size: Some(1),
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
            origin: None,
            unix: None,
        }
    }

    /// Only regular image files are looked up, in listing order; nothing
    /// here touches the disk.
    #[test]
    fn only_regular_images_are_looked_up_in_listing_order() {
        let mut link = image("link.png", 1);
        link.is_symlink = true;
        let mut folder = image("dir.png", 1);
        folder.kind = EntryKind::Directory;
        let snapshot = [
            image("b.jpg", 1),
            image("notes.txt", 1),
            link,
            folder,
            image("a.png", 2),
        ];
        let (keep, todo) = plan_thumbnails(&snapshot, &[], &Thumbs::new(), true, Bucket::Normal);
        assert!(keep.is_empty());
        let paths: Vec<_> = todo.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(
            paths,
            [PathBuf::from("/p/b.jpg"), PathBuf::from("/p/a.png")]
        );
    }

    /// A refresh keeps the answers that still apply on screen, looks a
    /// changed image up again and forgets the entries that left.
    #[test]
    fn a_refresh_keeps_known_answers_and_looks_up_what_changed() {
        let old = image("a.png", 1);
        let same = image("b.png", 1);
        let gone = image("gone.png", 1);
        let mut known = Thumbs::new();
        for entry in [&old, &same, &gone] {
            known.insert(
                entry.path.clone(),
                Thumb {
                    modified: entry.modified,
                    found: Some(PathBuf::from("/cache").join(&entry.name)),
                    misses: 0,
                },
            );
        }
        let snapshot = [image("a.png", 9), same.clone(), image("new.png", 1)];

        let (keep, todo) = plan_thumbnails(&snapshot, &[], &known, true, Bucket::Normal);

        assert!(
            keep.contains_key(Path::new("/p/a.png")),
            "stays until answered"
        );
        assert!(keep.contains_key(Path::new("/p/b.png")));
        assert!(
            !keep.contains_key(Path::new("/p/gone.png")),
            "left the listing"
        );
        let paths: Vec<_> = todo.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(
            paths,
            [PathBuf::from("/p/a.png"), PathBuf::from("/p/new.png")],
            "the unchanged image keeps its answer"
        );
    }

    /// The lookups follow the displayed order, so the tiles on screen answer
    /// first; images the view hides (or an order that is not known yet)
    /// follow in listing order, and a stale index is skipped.
    #[test]
    fn lookups_follow_the_displayed_order_then_the_rest() {
        let snapshot = [
            image("a.png", 1),
            image("b.png", 1),
            image("hidden.png", 1),
            image("c.png", 1),
        ];
        let displayed = [3, 1, 0, 9, 1];
        let (_, todo) =
            plan_thumbnails(&snapshot, &displayed, &Thumbs::new(), true, Bucket::Normal);
        let paths: Vec<_> = todo.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/p/c.png"),
                PathBuf::from("/p/b.png"),
                PathBuf::from("/p/a.png"),
                PathBuf::from("/p/hidden.png"),
            ]
        );
    }

    /// A thumbnail found in the cache's `normal` directory stays on screen
    /// when the zoom outgrows it and is looked up again for `large`; one that
    /// is big enough is never queued for a zoom change alone, in either
    /// direction.
    #[test]
    fn a_zoom_change_requeues_only_thumbnails_too_small_for_the_new_size() {
        let entry = image("a.png", 1);
        let known_in = |directory: &str| {
            let mut known = Thumbs::new();
            known.insert(
                entry.path.clone(),
                Thumb {
                    modified: entry.modified,
                    found: Some(PathBuf::from(format!("/c/thumbnails/{directory}/a.png"))),
                    misses: 0,
                },
            );
            known
        };
        let plan = |directory: &str, wanted| {
            let (keep, todo) = plan_thumbnails(
                std::slice::from_ref(&entry),
                &[0],
                &known_in(directory),
                true,
                wanted,
            );
            (keep.len(), todo.len())
        };
        assert_eq!(plan("normal", Bucket::Normal), (1, 0), "same size");
        assert_eq!(plan("normal", Bucket::Large), (1, 1), "kept, asked again");
        assert_eq!(plan("large", Bucket::Large), (1, 0), "already large");
        assert_eq!(plan("large", Bucket::Normal), (1, 0), "bigger is fine");
        assert_eq!(plan("x-large", Bucket::Large), (1, 0));
        assert_eq!(
            plan("elsewhere", Bucket::Large),
            (1, 0),
            "its own thumbnail"
        );
    }

    /// A settled miss stays settled across a zoom change: the bucket only
    /// matters for thumbnails that were found.
    #[test]
    fn a_zoom_change_does_not_wake_a_settled_miss() {
        let snapshot = [image("a.png", 1)];
        let mut known = Thumbs::new();
        pass_with_misses(&snapshot, &mut known);
        pass_with_misses(&snapshot, &mut known);
        let (_, todo) = plan_thumbnails(&snapshot, &[], &known, true, Bucket::Large);
        assert!(todo.is_empty());
    }

    /// The Recent list shows when a file was used, not when it changed, so
    /// nothing there is trusted.
    #[test]
    fn the_recent_list_looks_every_image_up_again() {
        let entry = image("a.png", 1);
        let mut known = Thumbs::new();
        known.insert(
            entry.path.clone(),
            Thumb {
                modified: entry.modified,
                found: None,
                misses: MAX_MISSES,
            },
        );
        let (keep, todo) = plan_thumbnails(
            std::slice::from_ref(&entry),
            &[0],
            &known,
            false,
            Bucket::Normal,
        );
        assert_eq!(keep.len(), 1);
        assert_eq!(todo.len(), 1);
    }

    /// Runs one planning pass the way the explorer does and records `outcome`
    /// (a miss) for every image it queued, returning how many it queued.
    fn pass_with_misses(snapshot: &[Entry], known: &mut Thumbs) -> usize {
        let (mut keep, todo) = plan_thumbnails(snapshot, &[], known, true, Bucket::Normal);
        for (path, modified) in &todo {
            let misses = misses_after(keep.get(path), *modified);
            keep.insert(
                path.clone(),
                Thumb {
                    modified: *modified,
                    found: None,
                    misses,
                },
            );
        }
        *known = keep;
        todo.len()
    }

    /// A first miss on an unchanged file is retried once.
    #[test]
    fn a_first_miss_on_an_unchanged_file_is_retried_once() {
        let snapshot = [image("a.png", 1)];
        let mut known = Thumbs::new();
        assert_eq!(pass_with_misses(&snapshot, &mut known), 1, "first lookup");
        assert_eq!(pass_with_misses(&snapshot, &mut known), 1, "the retry");
    }

    /// The second miss settles it: the file is not queued again.
    #[test]
    fn a_second_miss_on_the_same_unchanged_file_is_not_queued_again() {
        let snapshot = [image("a.png", 1)];
        let mut known = Thumbs::new();
        pass_with_misses(&snapshot, &mut known);
        pass_with_misses(&snapshot, &mut known);
        for _ in 0..3 {
            assert_eq!(pass_with_misses(&snapshot, &mut known), 0);
        }
        assert!(known.contains_key(Path::new("/p/a.png")), "glyph stays");
    }

    /// A changed file is looked up again after a settled miss, and gets its
    /// retry again.
    #[test]
    fn a_changed_file_is_queued_again_after_a_miss() {
        let mut known = Thumbs::new();
        let before = [image("a.png", 1)];
        pass_with_misses(&before, &mut known);
        pass_with_misses(&before, &mut known);
        assert_eq!(pass_with_misses(&before, &mut known), 0, "settled");

        let after = [image("a.png", 2)];
        assert_eq!(pass_with_misses(&after, &mut known), 1, "changed");
        assert_eq!(pass_with_misses(&after, &mut known), 1, "its own retry");
        assert_eq!(pass_with_misses(&after, &mut known), 0, "settled again");
    }

    /// A miss counts up only while the file is unchanged.
    #[test]
    fn misses_add_up_only_for_the_same_unchanged_file() {
        let t = |secs: u64, found: Option<&str>, misses| Thumb {
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
            found: found.map(PathBuf::from),
            misses,
        };
        let at = |secs: u64| Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs));
        assert_eq!(misses_after(None, at(1)), 1);
        assert_eq!(misses_after(Some(&t(1, None, 1)), at(1)), 2);
        assert_eq!(misses_after(Some(&t(1, None, 2)), at(2)), 1, "changed");
        assert_eq!(misses_after(Some(&t(1, Some("/c"), 0)), at(1)), 1);
    }

    /// A scratch directory under the temporary directory, unique per
    /// process, empty at the start and removed by the test that made it.
    #[allow(clippy::expect_used)]
    fn scratch(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fileflux-entries-test-{tag}-{}",
            std::process::id()
        ));
        drop(std::fs::remove_dir_all(&path));
        std::fs::create_dir_all(&path).expect("scratch directory");
        path
    }

    /// The theme-icon memo: two asks for the same (icon file, size) get
    /// the same rasterized image, so an icon is rasterized once per
    /// process — never again per frame per tile.
    #[test]
    #[allow(clippy::expect_used)]
    fn the_same_icon_and_size_rasterize_once() {
        let root = scratch("memo");
        let icon = root.join("red.svg");
        std::fs::write(
            &icon,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\"><rect width=\"64\" height=\"64\" fill=\"#ff0000\"/></svg>",
        )
        .expect("icon SVG");
        let first = rasterized(&icon, 32).expect("the icon rasterizes");
        let second = rasterized(&icon, 32).expect("the memo answers");
        assert!(
            Arc::ptr_eq(&first, &second),
            "the second ask reuses the first raster instead of rasterizing again"
        );
        let rasters = RASTERS.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(rasters.len(), 1, "exactly one (icon, size) was rasterized");
        drop(std::fs::remove_dir_all(&root));
    }
}
