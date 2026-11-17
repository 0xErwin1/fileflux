//! The entries area: the virtualized grid of folder tiles and the details
//! list, over `gpui::uniform_list`.
//!
//! The list and the grid both render chunked fixed-height rows; the grid
//! derives its column count from the content width with a minimum of one,
//! so no row renders the whole directory and no keystroke rescans it.
//! Clicks and keyboard arrows go through the raw entry paths the state
//! keeps; display names are lossy and never used for navigation.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use bolt::composites::ListRow;
use bolt::primitives::Icon;
use bolt::tokens::Spacing;
use fileflux::core::{Entry, EntryKind, Location};
use gpui::{
    App, ClickEvent, Context, Div, ElementId, InteractiveElement, IntoElement, Modifiers,
    ParentElement, Stateful, Styled, WeakEntity, Window, div, prelude::*, px,
};

use super::icon;
use super::state::View;
use super::util;
use super::view::{ENTRIES_CONTEXT, Explorer};

/// Tile width of the reference grid (normal density).
const TILE_WIDTH: f32 = 104.0;
/// Tile height, icon plus two filename lines.
const TILE_HEIGHT: f32 = 104.0;
/// Gap between tiles.
const TILE_GAP: f32 = 8.0;
/// Horizontal padding of the entries area.
const ENTRIES_PADDING: f32 = 16.0;

/// Derives the grid column count from the available content width, with a
/// minimum of one so nothing is ever unreachable.
pub(crate) fn grid_columns(available_width: f32) -> usize {
    let usable = (available_width - 2.0 * ENTRIES_PADDING).max(TILE_WIDTH);
    (((usable + TILE_GAP) / (TILE_WIDTH + TILE_GAP)).floor() as usize).max(1)
}

/// The entries area of the explorer: loading, empty, no-result and result
/// states.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let visible: Arc<Vec<usize>> = Arc::new(this.state.visible_indices());
    let count = visible.len();

    let mut container = div()
        .id("explorer-entries")
        .key_context(ENTRIES_CONTEXT)
        .track_focus(&this.entries_focus)
        .flex_1()
        .min_h_0()
        .min_w_0()
        .flex()
        .flex_col()
        .px(px(ENTRIES_PADDING))
        .pb(Spacing::XL)
        .overflow_y_scroll();

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

    let snapshot = Arc::clone(&this.snapshot);
    let selected: BTreeSet<PathBuf> = this.state.selection().clone();
    let columns = this.rendered_columns.max(1);
    let weak = cx.entity().downgrade();
    let list_view = this.state.view() == View::List;
    let item_count = if list_view {
        count
    } else {
        count.div_ceil(columns)
    };

    let list = gpui::uniform_list(
        "explorer-rows",
        item_count,
        move |row_range, _window, cx: &mut App| {
            row_range
                .map(|row| {
                    if list_view {
                        render_list_row(&snapshot, &visible, row, &selected, &weak, cx)
                    } else {
                        render_grid_row(&snapshot, &visible, row, columns, &selected, &weak, cx)
                    }
                })
                .collect::<Vec<_>>()
        },
    )
    .flex_1()
    .min_h_0()
    .track_scroll(&this.scroll.clone());

    if list_view {
        container = container
            .child(list_header(cx))
            .child(list.font_family(bolt::fonts::grid_family(cx)));
    } else {
        container = container.child(list);
    }

    container
}

/// The list header row: the four columns of the details list.
fn list_header(cx: &App) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    div()
        .id("explorer-list-header")
        .flex_none()
        .h(px(32.0))
        .flex()
        .items_center()
        .gap(Spacing::LG)
        .px(Spacing::MD)
        .border_b_1()
        .border_color(colors.line)
        .text_size(px(12.0))
        .text_color(colors.muted)
        .child(name_column().child("Name"))
        .child(kind_column().child("Type"))
        .child(size_column().child("Size"))
        .child(modified_column().child("Modified"))
}

/// The column widths of the details list, matching the reference design
/// (minmax(180px,1fr) 120px 100px 170px with 16px gaps).
fn name_column() -> Div {
    div().flex_1().min_w_0().truncate()
}

fn kind_column() -> Div {
    div().w(px(120.0)).flex_none().overflow_hidden()
}

fn size_column() -> Div {
    div().w(px(100.0)).flex_none().text_right()
}

fn modified_column() -> Div {
    div().w(px(170.0)).flex_none().overflow_hidden()
}

/// One details row at the visible position `row`.
fn render_list_row(
    snapshot: &[Entry],
    visible: &[usize],
    row: usize,
    selected: &BTreeSet<PathBuf>,
    weak: &WeakEntity<Explorer>,
    cx: &mut App,
) -> gpui::AnyElement {
    let colors = bolt::colors(cx);
    let fallback = || div().h(px(32.0));
    let Some(&index) = visible.get(row) else {
        return fallback().into_any_element();
    };
    let Some(entry) = snapshot.get(index) else {
        return fallback().into_any_element();
    };
    let path = entry.path.clone();
    let name = entry.display_name().into_owned();
    let kind = util::format_kind(entry);
    let size = util::format_size(entry.size);
    let modified = util::format_modified(entry.modified);
    let is_folder = entry.kind == EntryKind::Directory && !entry.is_symlink;
    let row_selected = selected.contains(&path);

    let row_element: Stateful<Div> =
        ListRow::new(ElementId::Name(format!("explorer-row-{row}").into()))
            .selected(row_selected)
            .selection_bar(true)
            .build(cx)
            .flex_none()
            .h(px(32.0))
            .flex()
            .items_center()
            .gap(Spacing::LG)
            .px(Spacing::MD)
            .border_b_1()
            .border_color(colors.row_divider)
            .on_click(entry_click(weak, path.clone(), name.clone()))
            .child(
                name_column()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(Spacing::SM)
                    .child(
                        Icon::new(icon(if is_folder {
                            gpui_kit_assets::IconName::Folder
                        } else if entry.kind == EntryKind::Symlink {
                            gpui_kit_assets::IconName::Link2Off
                        } else {
                            gpui_kit_assets::IconName::File
                        }))
                        .color(if is_folder {
                            colors.info
                        } else {
                            colors.muted
                        }),
                    )
                    .child(
                        div()
                            .text_color(if row_selected {
                                colors.strong
                            } else {
                                colors.foreground
                            })
                            .child(name),
                    ),
            )
            .child(kind_column().text_color(colors.muted).child(kind))
            .child(size_column().text_color(colors.muted).child(size))
            .child(modified_column().text_color(colors.muted).child(modified));

    row_element.into_any_element()
}

/// One grid row: up to `columns` tiles with fixed height and gap.
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
    let start = row * columns;
    let end = (start + columns).min(visible.len());
    let tiles: Vec<Entry> = visible
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .filter_map(|&index| snapshot.get(index))
        .cloned()
        .collect();

    let mut row_element = div()
        .id(ElementId::Name(format!("explorer-grid-row-{row}").into()))
        .flex()
        .gap(px(TILE_GAP))
        .h(px(TILE_HEIGHT))
        .min_w_0();
    for (tile_index, tile) in tiles.into_iter().enumerate() {
        let path = tile.path.clone();
        let name = tile.display_name().into_owned();
        let is_folder = tile.kind == EntryKind::Directory && !tile.is_symlink;
        let broken = tile.is_symlink && tile.kind == EntryKind::Symlink;
        let tile_selected = selected.contains(&path);

        row_element = row_element.child(
            ListRow::new(ElementId::Name(
                format!("explorer-tile-{row}-{tile_index}").into(),
            ))
            .selected(tile_selected)
            .selection_bar(true)
            .build(cx)
            .w_full()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(Spacing::LG)
            .pb(Spacing::SM)
            .px(Spacing::XS)
            .on_click(entry_click(weak, path.clone(), name.clone()))
            .child(
                Icon::new(icon(if is_folder {
                    gpui_kit_assets::IconName::Folder
                } else if broken {
                    gpui_kit_assets::IconName::Link2Off
                } else {
                    gpui_kit_assets::IconName::File
                }))
                .size(px(40.0))
                .color(if is_folder {
                    colors.info
                } else if broken {
                    colors.warning
                } else {
                    colors.muted
                }),
            )
            .child(
                div()
                    .mt(Spacing::MD)
                    .text_size(px(12.0))
                    .line_clamp(2)
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
/// the entries area, so the keyboard follows the pointer.
fn entry_click(
    weak: &WeakEntity<Explorer>,
    path: PathBuf,
    _name: String,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let weak = weak.clone();
    move |event: &ClickEvent, _window, cx| {
        let click_count = event.click_count();
        let modifiers = match event {
            ClickEvent::Mouse(mouse) => mouse.down.modifiers,
            _ => Modifiers::default(),
        };
        if let Err(error) = weak.update_in(cx, |this, window, cx| {
            this.entries_focus.focus(window, cx);
            if click_count >= 2 {
                this.open_path(&path, window, cx);
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
}
