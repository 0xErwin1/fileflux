//! The tab strip: a 30 px row at the top of the document island, drawn only
//! while two or more tabs are open.
//!
//! Square Bolt tabs (design/system.md: radius 0, 1 px lines, a 2 px tint
//! marker): the active tab sits on the raised fill with strong text and a
//! tint bar along its bottom edge; the others are muted text that lifts to
//! the raised fill on hover. A tab shows the folder's glyph and name (the
//! full path is its tooltip) and a close button while it is active or
//! hovered; a middle click closes it. Tabs share the row and shrink down to
//! a floor before they clip.
//!
//! A tab can be dragged (payload [`DraggedTab`], which the entry drop
//! targets ignore). While one is dragged, every tab shows two drop zones —
//! its left and right half, each a gap between tabs — and a tint bar marks
//! the gap the pointer is in; the empty end of the strip is the gap after
//! the last tab. In a split view a one-tab pane shows its strip meanwhile,
//! so a tab can be dropped onto the other pane.
//!
//! While exactly one folder is dragged from the entries the strip shows too
//! (a one-tab pane included) and takes the folder in the empty end of the
//! strip and in a narrow zone on each tab's edge — between tabs — opening it
//! in a new tab at that gap. The middle of a tab is not a drop zone: it does
//! nothing, as before.

use bolt::primitives::Icon;
use bolt::tokens::ChromeColors;
use fileflux::core::Location;
use gpui::{
    App, Context, Div, MouseButton, Render, SharedString, Stateful, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::ActiveTheme;
use gpui_component::tooltip::Tooltip;

use super::dnd::{self, DraggedEntries, DropZone};
use super::icon;
use super::panes::Side;
use super::tabs::{DraggedTab, dragged_tab, gap_is_noop, tab_label, tab_tooltip};
use super::view::Explorer;

/// Height of the strip and of every tab.
pub(crate) const TAB_HEIGHT: f32 = 30.0;
/// The narrowest the drag-time end zone of the strip gets.
const END_ZONE_MIN: f32 = 24.0;
/// The narrowest a tab shrinks to before the row clips.
const TAB_MIN_WIDTH: f32 = 96.0;
/// The widest a tab grows to.
const TAB_MAX_WIDTH: f32 = 220.0;
/// The width of the drop zone on each edge of a tab for a dragged folder.
const FOLDER_EDGE: f32 = 12.0;
/// Edge of the square close button.
const CLOSE_SIZE: f32 = 18.0;

/// Whether the strip of a one-tab pane is drawn as an overlay: only the drop
/// zones of a drag show it, so it lays over the toolbar instead of pushing
/// the entries down mid-drag.
fn is_overlay(this: &Explorer, cx: &App) -> bool {
    this.tabs.len() < 2 && is_drag_target(this, cx)
}

/// A drag the strip takes: a folder, or a tab of a split view (to the other
/// pane).
fn is_drag_target(this: &Explorer, cx: &App) -> bool {
    let tab_dragging = this.tab_drag.is_some() && cx.has_active_drag();
    (tab_dragging && this.tabs.is_split()) || dnd::folder_drag_live(this, cx)
}

/// The strip in the layout: nothing while there is a single tab.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Option<Stateful<Div>> {
    if is_overlay(this, cx) {
        return None;
    }
    build(this, cx)
}

/// The strip of a one-tab pane during a drag, over the top of the pane. The
/// caller adds it after the toolbar, so it paints above it.
pub(crate) fn overlay(this: &Explorer, cx: &mut Context<Explorer>) -> Option<Stateful<Div>> {
    if !is_overlay(this, cx) {
        return None;
    }
    let colors = bolt::colors(cx);
    build(this, cx).map(|strip| {
        strip
            .absolute()
            .inset_0()
            .h_full()
            .bg(colors.desk)
            .occlude()
    })
}

fn build(this: &Explorer, cx: &mut Context<Explorer>) -> Option<Stateful<Div>> {
    let tab_dragging = this.tab_drag.is_some() && cx.has_active_drag();
    let folder_dragging = dnd::folder_drag_live(this, cx);
    let dragging = tab_dragging || folder_dragging;
    if this.tabs.len() < 2 && !is_drag_target(this, cx) {
        return None;
    }
    // The dragged tab's pane and position, so the gaps that would leave the
    // order as it is show no zone.
    let grabbed = this
        .tab_drag
        .filter(|_| tab_dragging)
        .and_then(|id| this.tabs.locate(|tab| tab.id == id));
    // During a frame of a split view the focus is on the pane being drawn.
    let side = this.tabs.focus();
    let colors = bolt::colors(cx);
    let home = this
        .places
        .as_ref()
        .map(|places| places.home.as_path())
        .or(this.home.as_deref());
    let mut strip = div()
        .id("explorer-tabs")
        .flex_none()
        .flex()
        .items_stretch()
        .h(px(TAB_HEIGHT))
        .overflow_hidden()
        .border_b_1()
        .border_color(colors.line);
    // The tabs sit in a box of their own that is as wide as they would be
    // at their maximum, and shrinks when the strip is narrower: they get the
    // widths they have with no drag, and the end zone below takes only what
    // is left of the strip.
    let mut tabs = div()
        .h_full()
        .min_w_0()
        .flex()
        .items_stretch()
        .overflow_hidden()
        .w(px(TAB_MAX_WIDTH * this.tabs.len() as f32));
    let active = this.tabs.active_index();
    // Indices count window-wide (see `Panes::focused_offset`), so a click
    // can only reach a tab of the pane the strip belongs to.
    let offset = this.tabs.focused_offset();
    for (local, tab) in this.tabs.focused().iter().enumerate() {
        let index = offset + local;
        let location = tab.state.current();
        tabs = tabs.child(render_tab(
            index,
            Zones {
                id: tab.id,
                side,
                local,
                grabbed: tab_dragging.then_some(grabbed).flatten(),
                tab_dragging,
                folder_dragging,
            },
            index == active,
            location,
            tab_label(location, home),
            tab_tooltip(location, this.trash_root.as_deref()),
            cx,
        ));
    }
    strip = strip.child(tabs);
    if dragging {
        // The empty end of the strip: the gap after the last tab. It takes
        // the whole of what the tabs leave free (a grow factor below one
        // would leave it a sliver no pointer can hit), and never less than
        // END_ZONE_MIN: when the tabs fill the strip they give up that much,
        // split across all of them, so the end stays a target.
        let end = this.tabs.focused().len();
        let filler = div().h_full().flex_1().min_w(px(END_ZONE_MIN));
        let skip =
            grabbed.is_some_and(|(from_side, from)| from_side == side && gap_is_noop(from, end));
        if !skip {
            strip = strip.child(gap_zone(filler, side, end, Edge::Left, cx));
        }
    }
    Some(strip)
}

/// What a tab needs to draw its drop zones.
#[derive(Clone, Copy)]
struct Zones {
    /// The tab's identity.
    id: u64,
    side: Side,
    /// The tab's position in its own pane.
    local: usize,
    /// The dragged tab's pane and position, while a tab drag lasts.
    grabbed: Option<(Side, usize)>,
    /// A tab is being dragged: every tab shows its two halves as zones.
    tab_dragging: bool,
    /// One folder is being dragged: every tab shows a narrow zone on each
    /// edge.
    folder_dragging: bool,
}

/// Which edge of a zone the drop indicator is drawn on.
#[derive(Clone, Copy)]
enum Edge {
    Left,
    Right,
}

/// Makes `zone` accept a dragged tab for the gap `gap` of `side`'s strip: a
/// tint bar on `edge` while the pointer is over it, and a drop that moves
/// the tab there.
fn gap_zone<E>(zone: E, side: Side, gap: usize, edge: Edge, cx: &mut Context<Explorer>) -> E
where
    E: InteractiveElement + Styled,
{
    let tint = ChromeColors::tint(cx.theme());
    let marked = move |style: gpui::StyleRefinement| match edge {
        Edge::Left => style.border_l_2().border_color(tint),
        Edge::Right => style.border_r_2().border_color(tint),
    };
    zone.can_drop(|value, _, _| {
        dragged_tab(value).is_some() || dnd::dragged_folder(value).is_some()
    })
    .drag_over::<DraggedTab>(move |style, _, _, _| marked(style))
    .drag_over::<DraggedEntries>(move |style, _, _, _| marked(style))
    .on_drop::<DraggedEntries>(
        cx.listener(move |this, dragged: &DraggedEntries, window, cx| {
            this.drop_dragged(dragged, &DropZone::TabStrip { side, gap }, window, cx);
        }),
    )
    .on_drop::<DraggedTab>(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
        this.drop_tab(*dragged, side, gap, window, cx);
    }))
}

/// The card that follows the pointer while a tab is dragged.
struct TabDragPreview {
    label: String,
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bolt::colors(cx);
        let tint = ChromeColors::tint(cx.theme());
        div()
            .flex()
            .items_center()
            .h(px(TAB_HEIGHT))
            .w(px(TAB_MIN_WIDTH + 40.0))
            .px(px(12.0))
            .bg(colors.raised)
            .border_1()
            .border_color(tint)
            .text_size(px(13.0))
            .text_color(colors.strong)
            .child(div().min_w_0().truncate().child(self.label.clone()))
    }
}

/// One tab.
fn render_tab(
    index: usize,
    zones: Zones,
    active: bool,
    location: &Location,
    label: String,
    tooltip: String,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let group: SharedString = format!("explorer-tab-group-{index}").into();
    let glyph = match location {
        Location::Trash => gpui_kit_assets::IconName::Trash,
        Location::Recent => gpui_kit_assets::IconName::Clock,
        Location::Directory(_) => gpui_kit_assets::IconName::Folder,
        Location::Search { .. } => gpui_kit_assets::IconName::FolderSearch,
        Location::Usage { .. } => gpui_kit_assets::IconName::ChartPie,
    };
    let close_group = group.clone();
    let weak = cx.weak_entity();
    let drag_label = label.clone();
    let payload = DraggedTab {
        id: zones.id,
        side: zones.side,
    };
    div()
        .id(("explorer-tab", index))
        .group(group.clone())
        .relative()
        .flex_1()
        .min_w(px(TAB_MIN_WIDTH))
        .max_w(px(TAB_MAX_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .gap(px(8.0))
        .pl(px(12.0))
        .pr(px(6.0))
        .border_r_1()
        .border_color(colors.line)
        .text_size(px(13.0))
        .cursor_pointer()
        .when(active, |tab| {
            tab.bg(colors.raised).text_color(colors.strong)
        })
        .when(!active, |tab| {
            tab.text_color(colors.muted)
                .hover(|tab| tab.bg(colors.raised).text_color(colors.foreground))
        })
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .on_drag(payload, move |dragged, _grab, _window, cx| {
            let id = dragged.id;
            if let Err(error) = weak.update(cx, |this, cx| {
                this.tab_drag = Some(id);
                cx.notify();
            }) {
                log::debug!("a tab drag began in a closed explorer: {error}");
            }
            let label = drag_label.clone();
            cx.new(|_| TabDragPreview { label })
        })
        .on_click(cx.listener(move |this, _event, window, cx| {
            this.activate_tab(index, window, cx);
        }))
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(move |this, _event, window, cx| {
                this.close_tab(index, window, cx);
            }),
        )
        .child(Icon::new(icon(glyph)).size(px(14.0)).color(if active {
            tint
        } else {
            colors.muted
        }))
        .child(div().flex_1().min_w_0().truncate().child(label))
        .child(
            div()
                .id(("explorer-tab-close", index))
                .flex_none()
                .size(px(CLOSE_SIZE))
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.muted)
                .hover(|button| button.bg(colors.line).text_color(colors.strong))
                // Hidden (not removed) until the tab is active or hovered,
                // so the label never shifts.
                .when(!active, |button| {
                    button
                        .invisible()
                        .group_hover(close_group, |button| button.visible())
                })
                .tooltip(|window, cx| Tooltip::new("Close tab (Ctrl+W)").build(window, cx))
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.close_tab(index, window, cx);
                }))
                .child(Icon::new(icon(gpui_kit_assets::IconName::X)).size(px(12.0))),
        )
        .when(zones.tab_dragging || zones.folder_dragging, |tab| {
            let zone = |gap: usize, edge: Edge, cx: &mut Context<Explorer>| {
                let noop = zones
                    .grabbed
                    .is_some_and(|(side, from)| side == zones.side && gap_is_noop(from, gap));
                // Halves for a tab; a narrow edge for a folder, so the
                // middle of a tab stays no drop zone.
                let base = div().absolute().top_0().bottom_0();
                let base = if zones.tab_dragging {
                    base.w_1_2()
                } else {
                    base.w(px(FOLDER_EDGE))
                };
                let base = match edge {
                    Edge::Left => base.left_0(),
                    Edge::Right => base.right_0(),
                };
                (!noop).then(|| gap_zone(base, zones.side, gap, edge, cx))
            };
            tab.children(zone(zones.local, Edge::Left, cx))
                .children(zone(zones.local + 1, Edge::Right, cx))
        })
        .when(active, |tab| {
            // The 2 px tint marker along the bottom edge.
            tab.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(2.0))
                    .bg(tint),
            )
        })
}
