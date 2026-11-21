//! Rubber-band selection: dragging on the empty space of the entries area
//! draws a rectangle and selects what it covers.
//!
//! Hit testing is pure layout math over the fixed tile and row geometry —
//! never per-element bounds — so it stays cheap on huge folders and touches
//! no disk. Everything here works in **content coordinates**: the origin is
//! the top-left of the first row, and the scroll offset is already folded
//! in by the caller.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use bolt::tokens::ChromeColors;
use gpui::{
    AnyElement, App, Context, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Styled, Task, Window, div, px,
};
use gpui_component::ActiveTheme;

use super::dnd;
use super::panes::Side;
use super::state::SelectionSnapshot;
use super::state::View;
use super::view::Explorer;
use super::zoom::{GridZoomExt as _, ListZoomExt as _, TILE_GAP};
use fileflux::core::zoom::GridZoom;

/// How far, in logical pixels, the pointer must move from the press before a
/// press becomes a drag (a rectangle or a file drag): the desktop default
/// (`gtk-dnd-drag-threshold`). Shared by entry drags and the rubber band.
pub(crate) const DRAG_THRESHOLD: f32 = 8.0;
/// Depth of the zone along the top and bottom edges that scrolls the pane.
const EDGE_ZONE: f32 = 32.0;
/// Scroll step per tick at the inner border of the edge zone.
const EDGE_STEP: f32 = 24.0;
/// Width of the strip on the right, left to the pane's scrollbar.
pub(crate) const SCROLLBAR_STRIP: f32 = 14.0;

/// How entries are laid out in the content, enough to map points to entries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Layout {
    /// Fixed tiles of the `zoom` level, `columns` per row, left-aligned.
    Grid { columns: usize, zoom: GridZoom },
    /// Full-width rows of one height.
    List { row_height: f32 },
}

/// What a press landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Press {
    /// The whole cell of an entry: a list row or a grid tile, padding and
    /// all, the same area its hover wash covers.
    Entry(usize),
    /// Nothing: gaps between cells, margins, past the last column or row.
    Empty,
}

/// Classifies a press at a content point. A cell is the entry everywhere
/// inside it; only the gaps between cells and the margins are open space, so
/// a band never starts on a tile the user sees as an entry.
pub(crate) fn classify(layout: Layout, count: usize, x: f32, y: f32) -> Press {
    match entry_at(layout, count, x, y) {
        Some(index) => Press::Entry(index),
        None => Press::Empty,
    }
}

/// An axis-aligned rectangle in content coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) right: f32,
    pub(crate) bottom: f32,
}

impl Rect {
    /// The rectangle with two opposite corners, in either order.
    pub(crate) fn from_points(a: (f32, f32), b: (f32, f32)) -> Self {
        Self {
            left: a.0.min(b.0),
            top: a.1.min(b.1),
            right: a.0.max(b.0),
            bottom: a.1.max(b.1),
        }
    }
}

/// How the covered entries combine with the selection at press time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// No modifier: the covered entries become the selection.
    Replace,
    /// Ctrl: covered entries flip against the press-time selection.
    Toggle,
    /// Shift: covered entries are added to the press-time selection.
    Add,
}

impl Mode {
    /// The mode of a press with these modifiers (Ctrl wins over Shift).
    pub(crate) fn from_modifiers(control: bool, shift: bool) -> Self {
        if control {
            Mode::Toggle
        } else if shift {
            Mode::Add
        } else {
            Mode::Replace
        }
    }
}

/// The width and height of one tile at `zoom`.
fn tile_size(zoom: GridZoom) -> (f32, f32) {
    let metrics = zoom.metrics();
    (metrics.tile_width, metrics.tile_height())
}

/// The index of the entry under a content point, when there is one.
pub(crate) fn entry_at(layout: Layout, count: usize, x: f32, y: f32) -> Option<usize> {
    if x < 0.0 || y < 0.0 {
        return None;
    }
    let index = match layout {
        Layout::Grid { columns, zoom } => {
            let (tile_width, tile_height) = tile_size(zoom);
            let pitch = tile_width + TILE_GAP;
            let column = (x / pitch).floor() as usize;
            // The gap right of a tile is empty space.
            if column >= columns || x - column as f32 * pitch >= tile_width {
                return None;
            }
            (y / tile_height).floor() as usize * columns + column
        }
        Layout::List { row_height } => (y / row_height).floor() as usize,
    };
    (index < count).then_some(index)
}

/// The indices, in displayed order, of the entries `rect` intersects.
pub(crate) fn hits(layout: Layout, count: usize, rect: Rect) -> Vec<usize> {
    if count == 0 || rect.right < 0.0 || rect.bottom < 0.0 {
        return Vec::new();
    }
    let first_row = |row_height: f32| (rect.top / row_height).floor().max(0.0) as usize;
    let last_row = |row_height: f32| (rect.bottom / row_height).floor() as usize;
    match layout {
        Layout::List { row_height } => {
            let last = last_row(row_height).min(count - 1);
            (first_row(row_height)..=last).collect()
        }
        Layout::Grid { columns, zoom } => {
            let columns = columns.max(1);
            let (tile_width, tile_height) = tile_size(zoom);
            let pitch = tile_width + TILE_GAP;
            // A tile at column `c` spans `c * pitch .. c * pitch + tile_width`.
            let first_column = (((rect.left - tile_width) / pitch).floor() + 1.0).max(0.0) as usize;
            let last_column = ((rect.right / pitch).floor() as usize).min(columns - 1);
            let rows = first_row(tile_height)..=last_row(tile_height);
            let mut covered = Vec::new();
            for row in rows {
                let start = row * columns;
                if start >= count {
                    break;
                }
                covered.extend(
                    (first_column..=last_column)
                        .map(|c| start + c)
                        .filter(|&i| i < count),
                );
            }
            covered
        }
    }
}

/// The selection a rectangle yields: `covered` combined with the selection
/// the press started from (`base`) as `mode` says.
pub(crate) fn combine(
    mode: Mode,
    base: &BTreeSet<PathBuf>,
    covered: impl IntoIterator<Item = PathBuf>,
) -> BTreeSet<PathBuf> {
    let covered: BTreeSet<PathBuf> = covered.into_iter().collect();
    match mode {
        Mode::Replace => covered,
        Mode::Toggle => base.symmetric_difference(&covered).cloned().collect(),
        Mode::Add => base.union(&covered).cloned().collect(),
    }
}

/// Whether releasing the left button clears the selection: only a press on
/// empty space that never became a rectangle (it stayed under the drag
/// threshold) and carried no modifier. Ctrl and Shift clicks keep it.
pub(crate) fn release_clears_selection(rectangle: bool, mode: Mode) -> bool {
    !rectangle && mode == Mode::Replace
}

/// How far to scroll this tick for a pointer at window `pointer_y` over a
/// viewport spanning `top..bottom`: negative scrolls up, positive down, zero
/// away from the edges. It grows with how deep the pointer is in the edge
/// zone and keeps growing, to twice the step, once it is past the edge.
pub(crate) fn edge_scroll(pointer_y: f32, top: f32, bottom: f32) -> f32 {
    let zone = EDGE_ZONE.min((bottom - top) / 2.0);
    if zone <= 0.0 {
        return 0.0;
    }
    let ratio = |depth: f32| (depth / zone).min(2.0) * EDGE_STEP;
    if pointer_y < top + zone {
        -ratio(top + zone - pointer_y)
    } else if pointer_y > bottom - zone {
        ratio(pointer_y - (bottom - zone))
    } else {
        0.0
    }
}

/// Whether the pointer moved far enough from the press to start a drag.
pub(crate) fn past_threshold(press: (f32, f32), now: (f32, f32)) -> bool {
    let (dx, dy) = (now.0 - press.0, now.1 - press.1);
    dx.hypot(dy) >= DRAG_THRESHOLD
}

/// Time between autoscroll steps while the pointer rests near an edge.
const SCROLL_TICK: Duration = Duration::from_millis(16);
/// Alpha of the rectangle's fill and of its border.
const FILL_ALPHA: f32 = 0.14;
const BORDER_ALPHA: f32 = 0.7;

/// A press on empty space that may turn into a rectangle.
pub(crate) struct Band {
    side: Side,
    /// The press point in content coordinates: it stays on the same spot of
    /// the folder while the pane scrolls.
    anchor: (f32, f32),
    /// Window position of the press and of the pointer now.
    press: (f32, f32),
    pointer: (f32, f32),
    mode: Mode,
    /// The selection as it was at the press, restored by Escape.
    base: SelectionSnapshot,
    /// Whether the pointer left the press threshold: the press is then a
    /// rectangle, otherwise still a click.
    active: bool,
    /// The autoscroll loop; dropping the band stops it.
    _scroll: Option<Task<()>>,
}

/// Where the rows are on screen: the window rectangle they scroll in (the
/// list's excludes its header) and the current vertical scroll offset.
struct Viewport {
    /// Presses above this are not on the rows (the list's column header).
    min_y: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    /// At most zero: how far the content moved up.
    offset_y: f32,
}

impl Viewport {
    fn to_content(&self, point: (f32, f32)) -> (f32, f32) {
        (point.0 - self.left, point.1 - self.top - self.offset_y)
    }

    fn to_window(&self, point: (f32, f32)) -> (f32, f32) {
        (point.0 + self.left, point.1 + self.top + self.offset_y)
    }

    /// Whether a press here may start a band: anywhere in the pane,
    /// margins included, except the list header and the scrollbar strip.
    fn accepts(&self, point: (f32, f32)) -> bool {
        let on_scrollbar = point.0 >= self.right - SCROLLBAR_STRIP && point.0 < self.right;
        point.1 >= self.min_y && !on_scrollbar
    }
}

/// The entry a left press landed on, kept until the button goes up so a
/// release before the drag threshold can act as the click it was.
#[derive(Clone, Debug)]
pub(crate) struct EntryPress {
    path: PathBuf,
    was_selected: bool,
    click_count: usize,
    modifiers: gpui::Modifiers,
}

impl Explorer {
    /// The focused pane's rows: their window rectangle, scroll offset and
    /// layout, once the entries area has been drawn.
    fn rubber_viewport(&self, cx: &App) -> Option<(Viewport, Layout)> {
        let bounds = self.entries_bounds()?;
        let (header, handle, layout) = match self.tab().state.view() {
            View::Grid => (
                0.0,
                self.tab().grid_scroll.clone(),
                Layout::Grid {
                    columns: self.rendered_columns().max(1),
                    zoom: self.grid_zoom,
                },
            ),
            View::List => (
                f32::from(bolt::fonts::grid_header_height(cx)),
                self.tab().table.read(cx).vertical_scroll_handle.clone(),
                Layout::List {
                    row_height: self.list_zoom.row_height(),
                },
            ),
        };
        let offset_y = f32::from(handle.0.borrow().base_handle.offset().y).min(0.0);
        Some((
            Viewport {
                min_y: if header > 0.0 {
                    f32::from(bounds.top()) + header
                } else {
                    f32::NEG_INFINITY
                },
                left: f32::from(bounds.left()),
                top: f32::from(bounds.top()) + header,
                right: f32::from(bounds.right()),
                bottom: f32::from(bounds.bottom()),
                offset_y,
            },
            layout,
        ))
    }

    /// A left press in the entries area: on empty space it arms the band;
    /// on an entry it does nothing (the entry's own click and drag run).
    pub(crate) fn rubber_press(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.entry_press = None;
        // No active-drag check: a press never happens during an in-app drag
        // (the button is held), so a drag left over from an earlier gesture
        // must not keep empty-space clicks from working.
        if event.button != MouseButton::Left
            || self.dialogs.is_open()
            || self.menu.is_some()
            || self.renaming.is_some()
        {
            return;
        }
        let Some((viewport, layout)) = self.rubber_viewport(cx) else {
            return;
        };
        let pointer = (f32::from(event.position.x), f32::from(event.position.y));
        if !viewport.accepts(pointer) {
            return;
        }
        let (x, y) = viewport.to_content(pointer);
        let visible = self.tab().state.visible_shared();
        let target = classify(layout, visible.len(), x, y);
        if let Press::Entry(index) = target {
            // An entry: its own click and drag run; remember the press so a
            // drag that never left the threshold can still act as a click.
            self.entry_press = visible
                .get(index)
                .and_then(|&entry| self.tab().snapshot.get(entry))
                .map(|entry| EntryPress {
                    path: entry.path.clone(),
                    was_selected: self.tab().state.selection().contains(&entry.path),
                    click_count: event.click_count,
                    modifiers: event.modifiers,
                });
            dnd::press_gate(event.position, cx);
            return;
        }
        self.entries_focus().focus(window, cx);
        self.rubber_band = Some(Band {
            side: self.tabs.focus(),
            anchor: (x, y),
            press: pointer,
            pointer,
            mode: Mode::from_modifiers(event.modifiers.control, event.modifiers.shift),
            base: self.tab().state.selection_snapshot(),
            active: false,
            _scroll: None,
        });
    }

    /// An entry drag left the drag threshold: an unselected entry is
    /// selected alone now, as its click would have.
    fn entry_drag_armed(&mut self, cx: &mut Context<Self>) {
        let Some(press) = self
            .entry_press
            .as_mut()
            .filter(|press| !press.was_selected)
        else {
            return;
        };
        // Selected from now on, so the next move does nothing.
        press.was_selected = true;
        let path = press.path.clone();
        self.tab_mut().state.select_only(&path);
        cx.notify();
    }

    /// The pointer moved anywhere in the window (see the measuring canvas).
    pub(crate) fn rubber_move(
        &mut self,
        side: Side,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(band) = self.rubber_band.as_mut().filter(|band| band.side == side) else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) || self.tabs.focus() != side {
            // The release happened somewhere that never reached us.
            self.rubber_band = None;
            cx.notify();
            return;
        }
        band.pointer = (f32::from(event.position.x), f32::from(event.position.y));
        if !band.active {
            if !past_threshold(band.press, band.pointer) {
                return;
            }
            band.active = true;
            band._scroll = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(SCROLL_TICK).await;
                    match this.update(cx, |this, cx| this.rubber_scroll_tick(cx)) {
                        Ok(true) => {}
                        Ok(false) | Err(_) => return,
                    }
                }
            }));
        }
        self.rubber_apply(cx);
    }

    /// The left button went up: a rectangle keeps its selection; a press
    /// that never left the threshold is a plain click on empty space, which
    /// clears the selection (Ctrl and Shift clicks leave it alone).
    pub(crate) fn rubber_release(&mut self, side: Side, cx: &mut Context<Self>) {
        if self
            .rubber_band
            .as_ref()
            .is_none_or(|band| band.side != side)
        {
            return;
        }
        let Some(band) = self.rubber_band.take() else {
            return;
        };
        if release_clears_selection(band.active, band.mode) {
            self.tab_mut().state.clear_notice();
            self.tab_mut().state.clear_selection();
        }
        cx.notify();
    }

    /// Escape: drops the band and puts the press-time selection back.
    /// `true` when there was a band to cancel.
    pub(crate) fn cancel_rubber_band(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(band) = self.rubber_band.take() else {
            return false;
        };
        if band.active {
            self.tab_mut().state.restore_selection(band.base);
        }
        cx.notify();
        true
    }

    /// One autoscroll step; `false` once the band is gone.
    fn rubber_scroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(band) = self.rubber_band.as_ref().filter(|band| band.active) else {
            return false;
        };
        let pointer_y = band.pointer.1;
        let Some((viewport, _)) = self.rubber_viewport(cx) else {
            return true;
        };
        let step = edge_scroll(pointer_y, viewport.top, viewport.bottom);
        if step != 0.0 {
            let handle = match self.tab().state.view() {
                View::Grid => self.tab().grid_scroll.clone(),
                View::List => self.tab().table.read(cx).vertical_scroll_handle.clone(),
            };
            let base = handle.0.borrow().base_handle.clone();
            let mut offset = base.offset();
            let max = f32::from(base.max_offset().y).max(0.0);
            offset.y = px((viewport.offset_y - step).clamp(-max, 0.0));
            base.set_offset(offset);
            self.rubber_apply(cx);
        }
        true
    }

    /// Recomputes the selection from the rectangle between the anchor and
    /// the pointer.
    fn rubber_apply(&mut self, cx: &mut Context<Self>) {
        let Some(band) = self.rubber_band.take() else {
            return;
        };
        if let Some((viewport, layout)) = self.rubber_viewport(cx) {
            let visible = self.tab().state.visible_shared();
            let (px_, py_) = viewport.to_content(band.pointer);
            let rect = Rect::from_points(band.anchor, (px_, py_));
            let snapshot = std::sync::Arc::clone(&self.tab().snapshot);
            let covered = hits(layout, visible.len(), rect)
                .into_iter()
                .filter_map(|index| visible.get(index).and_then(|&entry| snapshot.get(entry)))
                .map(|entry| entry.path.clone());
            let selection = combine(band.mode, &band.base_selection(), covered);
            if &selection != self.tab().state.selection() {
                self.tab_mut().state.replace_selection(selection);
            }
            cx.notify();
        }
        self.rubber_band = Some(band);
    }

    /// The translucent rectangle over the rows of pane `side`, while its
    /// band is a rectangle; clipped to the rows area.
    pub(crate) fn rubber_overlay(&self, side: Side, cx: &App) -> Option<AnyElement> {
        let band = self
            .rubber_band
            .as_ref()
            .filter(|band| band.active && band.side == side)?;
        let (viewport, _) = self.rubber_viewport(cx)?;
        let bounds = self.entries_bounds()?;
        let corner = viewport.to_window(band.anchor);
        let rect = Rect::from_points(corner, band.pointer);
        let tint = ChromeColors::tint(cx.theme());
        let (origin_x, origin_y) = (f32::from(bounds.left()), f32::from(bounds.top()));
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .top(px(viewport.top - origin_y))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .left(px(rect.left - origin_x))
                        .top(px(rect.top - viewport.top))
                        .w(px(rect.right - rect.left))
                        .h(px(rect.bottom - rect.top))
                        .bg(tint.opacity(FILL_ALPHA))
                        .border_1()
                        .border_color(tint.opacity(BORDER_ALPHA)),
                )
                .into_any_element(),
        )
    }
}

impl Band {
    fn base_selection(&self) -> BTreeSet<PathBuf> {
        self.base.selected().clone()
    }
}

/// Registers the window-wide pointer listeners of pane `side`'s band; called
/// from the entries area's paint pass.
pub(crate) fn listen(weak: &gpui::WeakEntity<Explorer>, side: Side, window: &mut Window) {
    let moves = weak.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
        if phase != gpui::DispatchPhase::Capture {
            return;
        }
        if cx.has_active_drag()
            && dnd::observe_gate(event.position, cx).is_armed()
            && let Err(error) = moves.update(cx, |this, cx| this.entry_drag_armed(cx))
        {
            log::debug!("the entries area outlived its explorer: {error}");
        }
        if let Err(error) = moves.update(cx, |this, cx| this.rubber_move(side, event, cx)) {
            log::debug!("the entries area outlived its explorer: {error}");
        }
    });
    let wheel = weak.clone();
    window.on_mouse_event(move |event: &gpui::ScrollWheelEvent, phase, _window, cx| {
        // Captured before the list under the pointer scrolls: Ctrl+wheel
        // zooms and must not also scroll.
        if phase != gpui::DispatchPhase::Capture {
            return;
        }
        match wheel.update(cx, |this, cx| this.wheel_zoom(side, event, cx)) {
            Ok(true) => cx.stop_propagation(),
            Ok(false) => {}
            Err(error) => log::debug!("the entries area outlived its explorer: {error}"),
        }
    });
    let ups = weak.clone();
    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
        if phase != gpui::DispatchPhase::Capture || event.button != MouseButton::Left {
            return;
        }
        let gate = dnd::finish_gate(event.position, cx);
        let press = ups
            .update(cx, |this, _| this.entry_press.take())
            .ok()
            .flatten();
        if dnd::release_is_plain_click(gate, cx.has_active_drag())
            && let Some(press) = press
        {
            // Released before the drag threshold: the click it was.
            super::entries::handle_entry_click(
                &ups,
                press.path,
                press.click_count,
                press.modifiers,
                window,
                cx,
            );
        }
        if let Err(error) = ups.update(cx, |this, cx| this.rubber_release(side, cx)) {
            log::debug!("the entries area outlived its explorer: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::zoom::TILE_TOP_PADDING;

    /// Today's tile, the Medium level.
    const TILE_WIDTH: f32 = 112.0;
    const TILE_HEIGHT: f32 = 126.0;
    const GRID: Layout = Layout::Grid {
        columns: 4,
        zoom: GridZoom::Medium,
    };
    const LIST: Layout = Layout::List { row_height: 30.0 };
    const PITCH: f32 = TILE_WIDTH + TILE_GAP;

    fn paths(names: &[&str]) -> BTreeSet<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn rect(left: f32, top: f32, right: f32, bottom: f32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn rect_from_points_orders_the_corners() {
        assert_eq!(
            Rect::from_points((10.0, 40.0), (3.0, 5.0)),
            rect(3.0, 5.0, 10.0, 40.0)
        );
    }

    #[test]
    fn grid_hits_cover_exactly_the_intersected_tiles() {
        // Ten tiles in rows of four: the third row holds two.
        let two_by_two = rect(PITCH + 4.0, 4.0, 2.0 * PITCH + 4.0, TILE_HEIGHT + 4.0);
        assert_eq!(hits(GRID, 10, two_by_two), vec![1, 2, 5, 6]);
        // A rectangle inside one tile hits that tile alone.
        assert_eq!(hits(GRID, 10, rect(5.0, 5.0, 9.0, 9.0)), vec![0]);
        // Whole area: every entry, the partial last row included.
        assert_eq!(
            hits(GRID, 10, rect(0.0, 0.0, 10_000.0, 10_000.0)),
            (0..10).collect::<Vec<_>>()
        );
        // Past the last entry of the partial row there is nothing.
        let empty_cells = rect(
            2.0 * PITCH,
            2.0 * TILE_HEIGHT,
            3.0 * PITCH,
            3.0 * TILE_HEIGHT,
        );
        assert!(hits(GRID, 10, empty_cells).is_empty());
    }

    #[test]
    fn grid_hits_skip_the_gap_between_tiles_and_clip_negative_space() {
        let gap = rect(TILE_WIDTH + 1.0, 5.0, TILE_WIDTH + TILE_GAP - 1.0, 9.0);
        assert!(hits(GRID, 10, gap).is_empty(), "the gap holds no tile");
        // A rectangle hanging off the top-left still hits the first tile.
        assert_eq!(hits(GRID, 10, rect(-50.0, -50.0, 3.0, 3.0)), vec![0]);
        assert!(hits(GRID, 10, rect(-50.0, -50.0, -1.0, -1.0)).is_empty());
        assert!(hits(GRID, 0, rect(0.0, 0.0, 99.0, 99.0)).is_empty());
    }

    #[test]
    fn grid_hits_stay_inside_the_column_count() {
        let wide = rect(0.0, 0.0, 100.0 * PITCH, 5.0);
        assert_eq!(hits(GRID, 10, wide), vec![0, 1, 2, 3]);
        // One column: tiles stack.
        let one = Layout::Grid {
            columns: 1,
            zoom: GridZoom::Medium,
        };
        assert_eq!(
            hits(one, 3, rect(0.0, 0.0, 500.0, 2.0 * TILE_HEIGHT - 1.0)),
            vec![0, 1]
        );
    }

    #[test]
    fn list_hits_are_the_rows_the_rectangle_spans() {
        assert_eq!(hits(LIST, 10, rect(0.0, 35.0, 5.0, 65.0)), vec![1, 2]);
        assert_eq!(hits(LIST, 10, rect(0.0, 10.0, 5.0, 12.0)), vec![0]);
        assert_eq!(hits(LIST, 3, rect(0.0, 0.0, 5.0, 900.0)), vec![0, 1, 2]);
        assert!(hits(LIST, 3, rect(0.0, 100.0, 5.0, 900.0)).is_empty());
        assert!(hits(LIST, 3, rect(0.0, -9.0, 5.0, -1.0)).is_empty());
    }

    #[test]
    fn a_million_entries_are_hit_by_arithmetic() {
        let near_end = rect(0.0, 29_999_970.0, 5.0, 29_999_999.0);
        assert_eq!(hits(LIST, 1_000_000, near_end), vec![999_999]);
        let tiles = Layout::Grid {
            columns: 10,
            zoom: GridZoom::Medium,
        };
        let row = 99_999.0 * TILE_HEIGHT;
        assert_eq!(
            hits(tiles, 1_000_000, rect(0.0, row + 1.0, 3.0, row + 2.0)),
            vec![999_990]
        );
    }

    /// The hit test follows the level's tile, not a fixed one.
    #[test]
    fn hit_testing_uses_the_tile_of_the_zoom_level() {
        for zoom in GridZoom::ALL {
            let metrics = zoom.metrics();
            let (width, height) = (metrics.tile_width, metrics.tile_height());
            let layout = Layout::Grid { columns: 3, zoom };
            let pitch = width + TILE_GAP;
            assert_eq!(entry_at(layout, 9, 1.0, 1.0), Some(0), "{zoom:?}");
            assert_eq!(
                entry_at(layout, 9, pitch + 1.0, height + 1.0),
                Some(4),
                "{zoom:?}"
            );
            assert_eq!(entry_at(layout, 9, width + 1.0, 1.0), None, "{zoom:?} gap");
            let two_by_two = rect(pitch + 1.0, 1.0, 2.0 * pitch + 1.0, height + 1.0);
            assert_eq!(hits(layout, 9, two_by_two), vec![1, 2, 4, 5], "{zoom:?}");
        }
    }

    #[test]
    fn entry_at_tells_entries_from_empty_space() {
        assert_eq!(entry_at(GRID, 10, 5.0, 5.0), Some(0));
        assert_eq!(entry_at(GRID, 10, PITCH + 1.0, TILE_HEIGHT + 1.0), Some(5));
        assert_eq!(entry_at(GRID, 10, TILE_WIDTH + 2.0, 5.0), None, "gap");
        assert_eq!(entry_at(GRID, 10, 5.0, 3.0 * TILE_HEIGHT), None, "below");
        assert_eq!(
            entry_at(GRID, 10, 2.0 * PITCH + 1.0, 2.0 * TILE_HEIGHT + 1.0),
            None,
            "partial row"
        );
        assert_eq!(
            entry_at(GRID, 10, 4.0 * PITCH + 1.0, 5.0),
            None,
            "past the columns"
        );
        assert_eq!(entry_at(GRID, 10, -1.0, 5.0), None);
        assert_eq!(entry_at(LIST, 3, 400.0, 65.0), Some(2));
        assert_eq!(entry_at(LIST, 3, 400.0, 95.0), None);
        assert_eq!(entry_at(LIST, 3, 400.0, -1.0), None);
    }

    #[test]
    fn modifiers_choose_replace_toggle_or_add() {
        assert_eq!(Mode::from_modifiers(false, false), Mode::Replace);
        assert_eq!(Mode::from_modifiers(true, false), Mode::Toggle);
        assert_eq!(Mode::from_modifiers(false, true), Mode::Add);
        assert_eq!(Mode::from_modifiers(true, true), Mode::Toggle);
    }

    #[test]
    fn combine_applies_each_mode_against_the_press_time_selection() {
        let base = paths(&["a", "b"]);
        let covered = || [PathBuf::from("b"), PathBuf::from("c")];
        assert_eq!(combine(Mode::Replace, &base, covered()), paths(&["b", "c"]));
        assert_eq!(combine(Mode::Toggle, &base, covered()), paths(&["a", "c"]));
        assert_eq!(
            combine(Mode::Add, &base, covered()),
            paths(&["a", "b", "c"])
        );
        // Shrinking the rectangle gives the base back: nothing accumulates.
        for mode in [Mode::Toggle, Mode::Add] {
            assert_eq!(combine(mode, &base, []), base);
        }
        assert!(combine(Mode::Replace, &base, []).is_empty());
    }

    #[test]
    fn edge_scroll_is_zero_inside_and_ramps_toward_and_past_the_edges() {
        let (top, bottom) = (100.0, 500.0);
        assert_eq!(edge_scroll(300.0, top, bottom), 0.0);
        assert_eq!(edge_scroll(top + EDGE_ZONE, top, bottom), 0.0);
        assert_eq!(edge_scroll(bottom - EDGE_ZONE, top, bottom), 0.0);
        assert_eq!(edge_scroll(top, top, bottom), -EDGE_STEP);
        assert_eq!(edge_scroll(bottom, top, bottom), EDGE_STEP);
        let half = edge_scroll(top + EDGE_ZONE / 2.0, top, bottom);
        assert!((half + EDGE_STEP / 2.0).abs() < 1e-4, "{half}");
        assert_eq!(edge_scroll(top - EDGE_ZONE, top, bottom), -2.0 * EDGE_STEP);
        assert_eq!(
            edge_scroll(top - 1000.0, top, bottom),
            -2.0 * EDGE_STEP,
            "capped"
        );
        assert_eq!(edge_scroll(bottom + 1000.0, top, bottom), 2.0 * EDGE_STEP);
    }

    #[test]
    fn edge_scroll_in_a_tiny_viewport_never_scrolls_both_ways() {
        let step = edge_scroll(10.0, 0.0, 20.0);
        assert_eq!(step, 0.0, "the exact middle is neutral");
        assert!(edge_scroll(1.0, 0.0, 20.0) < 0.0);
        assert!(edge_scroll(19.0, 0.0, 20.0) > 0.0);
    }

    #[test]
    fn a_press_becomes_a_drag_only_past_the_threshold() {
        assert_eq!(DRAG_THRESHOLD, 8.0);
        let press = (100.0, 100.0);
        assert!(!past_threshold(press, press));
        assert!(!past_threshold(press, (107.0, 100.0)));
        assert!(!past_threshold(press, (105.0, 105.0)), "diagonal 7.07");
        assert!(past_threshold(press, (108.0, 100.0)));
        assert!(past_threshold(press, (100.0, 91.0)));
        assert!(past_threshold(press, (106.0, 106.0)), "diagonal 8.49");
    }

    #[test]
    fn a_tile_is_the_entry_everywhere_inside_it() {
        // The user's report: clicking the tile's padding around the icon and
        // name used to do nothing, only the drawn art selected.
        let icon = (TILE_WIDTH / 2.0, TILE_TOP_PADDING + 10.0);
        assert_eq!(classify(GRID, 10, icon.0, icon.1), Press::Entry(0));
        assert_eq!(classify(GRID, 10, 3.0, 3.0), Press::Entry(0), "corner");
        assert_eq!(
            classify(GRID, 10, TILE_WIDTH - 1.0, TILE_HEIGHT - 1.0),
            Press::Entry(0),
            "opposite corner"
        );
        assert_eq!(
            classify(GRID, 10, PITCH + icon.0, TILE_HEIGHT + icon.1),
            Press::Entry(5)
        );
    }

    #[test]
    fn classify_tells_cells_from_gaps_and_margins() {
        let icon = (TILE_WIDTH / 2.0, TILE_TOP_PADDING + 10.0);
        assert_eq!(classify(GRID, 10, TILE_WIDTH + 2.0, icon.1), Press::Empty);
        assert_eq!(classify(GRID, 10, -5.0, icon.1), Press::Empty, "margin");
        assert_eq!(classify(GRID, 10, icon.0, -5.0), Press::Empty, "top margin");
        assert_eq!(classify(GRID, 10, icon.0, 5.0 * TILE_HEIGHT), Press::Empty);
        assert_eq!(
            classify(GRID, 10, 2.0 * PITCH + icon.0, 2.0 * TILE_HEIGHT + icon.1),
            Press::Empty,
            "past the partial last row"
        );
        assert_eq!(classify(LIST, 3, 500.0, 65.0), Press::Entry(2));
        assert_eq!(classify(LIST, 3, 500.0, 95.0), Press::Empty);
    }

    #[test]
    fn viewport_maps_presses_through_the_scroll_offset() {
        let viewport = Viewport {
            left: 300.0,
            top: 100.0,
            right: 1000.0,
            bottom: 800.0,
            offset_y: -2.0 * TILE_HEIGHT,
            min_y: f32::NEG_INFINITY,
        };
        // Window y 100 + 10 is 2 rows plus 10 px down the folder.
        let (x, y) = viewport.to_content((300.0 + 5.0, 110.0));
        assert_eq!((x, y), (5.0, 2.0 * TILE_HEIGHT + 10.0));
        assert_eq!(classify(GRID, 100, x, y), Press::Entry(2 * 4));
        assert_eq!(viewport.to_window((x, y)), (305.0, 110.0));
    }

    #[test]
    fn viewport_accepts_margins_but_not_header_or_scrollbar() {
        let grid = Viewport {
            left: 300.0,
            top: 100.0,
            right: 1000.0,
            bottom: 800.0,
            offset_y: 0.0,
            min_y: f32::NEG_INFINITY,
        };
        assert!(grid.accepts((290.0, 96.0)), "left and top margins");
        assert!(grid.accepts((1010.0, 500.0)), "right margin");
        assert!(grid.accepts((500.0, 830.0)), "bottom margin");
        assert!(!grid.accepts((1000.0 - 5.0, 500.0)), "scrollbar strip");
        let list = Viewport {
            min_y: 100.0,
            ..grid
        };
        assert!(!list.accepts((500.0, 90.0)), "column header");
        assert!(list.accepts((500.0, 100.0)));
    }

    #[test]
    fn a_plain_click_on_empty_space_clears_the_selection() {
        // Pressed and released under the threshold: never a rectangle.
        assert!(release_clears_selection(false, Mode::Replace));
    }

    #[test]
    fn ctrl_and_shift_clicks_on_empty_space_keep_the_selection() {
        assert!(!release_clears_selection(false, Mode::Toggle));
        assert!(!release_clears_selection(false, Mode::Add));
    }

    #[test]
    fn a_finished_rectangle_keeps_what_it_selected() {
        for mode in [Mode::Replace, Mode::Toggle, Mode::Add] {
            assert!(!release_clears_selection(true, mode));
        }
    }
}
