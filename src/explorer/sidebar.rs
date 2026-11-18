//! The sidebar island: standard places, the favorites and the
//! mounted devices, discovered off the UI thread.
//!
//! Rows show the name only and navigate on click; the full path is the
//! row's tooltip and the active row shows the 2 px tint bar of the
//! reference design. Favorites are saved in the settings file and
//! are added and removed for the current directory with the toolbar's star
//! button; the virtual Trash is never favorited. Devices add a 3 px usage
//! meter and a "free of" line only when `statvfs` answered; nothing is
//! invented otherwise.

use bolt::composites::{Island, ListRow};
use bolt::primitives::Icon;
use bolt::tokens::{ChromeColors, IslandMetrics, Spacing, TreeMetrics};
use fileflux::core::Location;
use fileflux::core::places::Volume;
use std::path::Path;

use fileflux::core::settings::{DETAILS_WIDTH_RANGE, SIDEBAR_WIDTH_RANGE};
use gpui::{
    App, Context, Div, ElementId, MouseButton, MouseDownEvent, ParentElement, Pixels, Stateful,
    Styled, Window, div, prelude::*, px, relative,
};
use gpui_component::{ActiveTheme, tooltip::Tooltip};

use super::actions::MenuOn;
use super::dnd::{self, DropTarget};
use super::entries;
use super::icon;
use super::state::Transition;
use super::tabs::Placement;
use super::view::Explorer;

/// The sidebar island, or nothing while it is toggled closed — at either
/// window width, since a collapsed sidebar must not resurrect on a resize.
pub(crate) fn render(
    this: &Explorer,
    narrow: bool,
    cx: &mut Context<Explorer>,
) -> Option<Stateful<Div>> {
    if !this.sidebar_open {
        return None;
    }
    let colors = bolt::colors(cx);

    let mut island = Island::new()
        .w(px(this.sidebar_width))
        .h_full()
        .child(places_section(this, cx))
        .child(favorites_section(this, cx))
        .child(volumes_section(this, cx));

    if let Some(error) = &this.sidebar_error {
        island = island.child(
            div()
                .id("explorer-sidebar-error")
                .px(Spacing::LG)
                .py(Spacing::MD)
                .text_size(px(12.0))
                .text_color(colors.warning)
                .child(format!(
                    "Places could not be discovered: {error}. The root stays reachable."
                )),
        );
    }

    if narrow {
        // The parked sidebar overlays the content island from the left.
        return Some(
            div()
                .id("explorer-sidebar-overlay")
                .absolute()
                .left(IslandMetrics::GAP)
                .top(IslandMetrics::GAP)
                .bottom(px(46.0))
                .w(px(this.sidebar_width))
                .overflow_y_scroll()
                .child(island),
        );
    }

    Some(
        div()
            .id("explorer-sidebar-slot")
            .flex_none()
            .w(px(this.sidebar_width))
            .h_full()
            .overflow_y_scroll()
            .child(island),
    )
}

/// One section label.
fn section_label(id: &'static str, text: &str, cx: &App) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    div()
        .id(id)
        .mt(Spacing::XL)
        .mb(Spacing::SM)
        .px(Spacing::LG)
        .text_size(px(11.0))
        .text_color(colors.muted)
        .child(text.to_owned())
}

/// The Places section: Home, the user directories and the scoped Trash.
fn places_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let colors = bolt::colors(cx);
    let mut section = div().flex_none().flex().flex_col();
    if this.places.is_none() && this.sidebar_error.is_none() {
        // Discovery is still running; say so instead of an empty column.
        return section.child(
            div()
                .id("explorer-sidebar-loading")
                .px(Spacing::LG)
                .pt(Spacing::MD)
                .text_size(px(12.0))
                .text_color(colors.muted)
                .child("Discovering places…"),
        );
    }
    section = section.child(section_label("places-label", "Places", cx));
    let Some(places) = this.places.clone() else {
        return section;
    };
    for (index, place) in places.places.iter().enumerate() {
        let icon = match place.label.as_str() {
            "Home" => gpui_kit_assets::IconName::House,
            "Desktop" => gpui_kit_assets::IconName::Monitor,
            "Documents" | "Downloads" => gpui_kit_assets::IconName::File,
            "Music" => gpui_kit_assets::IconName::Music,
            "Pictures" => gpui_kit_assets::IconName::Image,
            _ => gpui_kit_assets::IconName::Folder,
        };
        section = section.child(nav_row(
            format!("place-{index}"),
            &place.label,
            place.path.to_string_lossy().into_owned(),
            icon,
            &Location::Directory(place.path.clone()),
            Some(places.home.as_path()),
            this,
            cx,
        ));
    }
    section.child(nav_row(
        "place-trash".to_owned(),
        "Home Trash",
        this.trash_root
            .as_ref()
            .map_or_else(String::new, |root| root.to_string_lossy().into_owned()),
        gpui_kit_assets::IconName::Trash,
        &Location::Trash,
        Some(places.home.as_path()),
        this,
        cx,
    ))
}

/// The Favorites section: saved rows, with an empty hint.
fn favorites_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let mut section = div().flex_none().flex().flex_col();
    section = section.child(section_label("favorites-label", "Favorites", cx));
    if this.favorites.list().is_empty() {
        let colors = bolt::colors(cx);
        return section.child(
            div()
                .id("explorer-favorites-empty")
                .mx(Spacing::LG)
                .mt(px(2.0))
                .px(px(10.0))
                .py(Spacing::MD)
                .border_1()
                .border_dashed()
                .border_color(colors.line_2)
                .text_size(px(12.0))
                .line_height(px(17.0))
                .text_color(colors.muted)
                .child("Star a folder to pin it here."),
        );
    }
    for (index, path) in this.favorites.list().iter().enumerate() {
        section = section.child(nav_row(
            format!("favorite-{index}"),
            &path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
            path.to_string_lossy().into_owned(),
            gpui_kit_assets::IconName::Star,
            &Location::Directory(path.clone()),
            None,
            this,
            cx,
        ));
    }
    section
}

/// The Devices section: the mount table's real storage volumes with a
/// usage meter when `statvfs` answered.
fn volumes_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let mut section = div().flex_none().flex().flex_col().pb(Spacing::MD);
    section = section.child(section_label("volumes-label", "Devices", cx));
    let Some(places) = this.places.clone() else {
        return section;
    };
    for (index, volume) in places.volumes.iter().enumerate() {
        section = section.child(volume_row(index, volume, places.home.as_path(), this, cx));
    }
    section
}

/// One device row: icon and name, then the 3 px meter and the mono
/// "free of" line, both left out without usage numbers.
fn volume_row(
    index: usize,
    volume: &Volume,
    home: &Path,
    this: &Explorer,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let location = Location::Directory(volume.path.clone());
    let active = this.tab().state.current() == &location;
    let glyph = if volume.removable {
        gpui_kit_assets::IconName::Usb
    } else {
        gpui_kit_assets::IconName::HardDrive
    };
    let name = if volume.path == home {
        "Home".to_owned()
    } else {
        volume.label.clone()
    };
    let name_for_menu = name.clone();
    let top = div()
        .flex()
        .items_center()
        .gap(Spacing::SM)
        .child(
            Icon::new(icon(glyph))
                .size(TreeMetrics::ICON)
                .color(if active { tint } else { colors.muted }),
        )
        .child(name);
    let mut row = row_shell(
        this,
        format!("volume-{index}"),
        name_for_menu,
        volume.path.to_string_lossy().into_owned(),
        location,
        active,
        cx,
    )
    .py(Spacing::SM)
    .flex_col()
    .items_stretch()
    .gap(px(5.0))
    .child(top);
    if let Some(usage) = volume.usage {
        let fill = if usage.is_high() {
            colors.warning
        } else {
            colors.info
        };
        let indent = px(25.0);
        row = row
            .child(
                div().ml(indent).h(px(3.0)).bg(colors.raised).child(
                    div()
                        .h_full()
                        .w(relative(usage.used_fraction() as f32))
                        .bg(fill),
                ),
            )
            .child(
                div()
                    .ml(indent)
                    .font_family(bolt::fonts::editor_family(cx))
                    .text_size(px(11.0))
                    .text_color(colors.muted)
                    .child(usage.label()),
            );
    }
    row
}

/// The shared row chrome: selection bar, click navigation and the full
/// path as the tooltip (skipped when there is no path to show).
fn row_shell(
    this: &Explorer,
    id: String,
    name: String,
    path: String,
    location: Location,
    active: bool,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    // The row its context menu is open on keeps the tint outline.
    let outlined = matches!(
        this.menu.as_ref().map(|menu| &menu.on),
        Some(MenuOn::Sidebar { row, .. }) if *row == id
    );
    let menu_row = id.clone();
    let menu_location = location.clone();
    // Dropping on a row is dropping on its place: a folder, or the Trash.
    let target = match &location {
        Location::Directory(path) => DropTarget::Folder(path.clone()),
        Location::Trash => DropTarget::Trash,
    };
    let row = ListRow::new(ElementId::Name(id.clone().into()))
        .selected(active)
        .selection_bar(true)
        // Bolt's own hover wash is 4 %: the row draws a visible one below.
        .interactive(false)
        .build(cx)
        .cursor_pointer()
        .flex_none()
        .min_h(px(26.0))
        .flex()
        .items_center()
        .gap(Spacing::SM)
        .px(Spacing::LG)
        // Transparent until a drop is hovering, so the highlight never
        // moves the row.
        .border_1()
        .border_color(if outlined {
            tint.opacity(entries::CONTEXT_OUTLINE_ALPHA)
        } else {
            gpui::transparent_black()
        })
        .when(outlined, |row| {
            row.bg(tint.opacity(entries::CONTEXT_WASH_ALPHA))
        })
        .when(!active && !outlined, |row| {
            row.hover(|row| row.bg(entries::hover_wash(&colors)))
        })
        .text_size(px(13.0))
        .text_color(if active {
            colors.strong
        } else {
            colors.foreground
        })
        .on_click(cx.listener({
            let location = location.clone();
            move |this, _event, window, cx| {
                this.navigate(Transition::Visit(location.clone()), window, cx);
            }
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.open_sidebar_menu(
                    event.position,
                    menu_row.clone(),
                    name.clone(),
                    menu_location.clone(),
                    window,
                    cx,
                );
            }),
        )
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(move |this, _event, window, cx| {
                this.open_tab(location.clone(), Placement::Background, window, cx);
            }),
        );
    let row = dnd::drop_target(row, target, &cx.entity().downgrade(), cx);
    // No path tooltip while a menu is open: it would sit on top of the
    // menu's header.
    if path.is_empty() || this.menu.is_some() {
        return row;
    }
    row.tooltip(move |window, cx| Tooltip::new(path.clone()).build(window, cx))
}

/// One sidebar navigation row: icon and name, with the exact path as the
/// tooltip and the active treatment for the current location.
#[allow(clippy::too_many_arguments)]
fn nav_row(
    id: String,
    label: &str,
    path: String,
    glyph: gpui_kit_assets::IconName,
    location: &Location,
    home: Option<&Path>,
    this: &Explorer,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let active = this.tab().state.current() == location;
    let home_label = home.is_some_and(|home| location == &Location::Directory(home.to_path_buf()));
    let shown_label = if home_label {
        "Home".to_owned()
    } else {
        label.to_owned()
    };
    row_shell(
        this,
        id,
        shown_label.clone(),
        path,
        location.clone(),
        active,
        cx,
    )
    .child(
        Icon::new(icon(glyph))
            .size(TreeMetrics::ICON)
            .color(if active { tint } else { colors.muted }),
    )
    .child(shown_label)
}

// The favorites rows show their own file names; no home rewriting, so
// their `home` comparison input stays unset.

// --- resizable panels ---------------------------------------------------------

/// The pointer hit area of a panel's drag handle, in pixels: wide enough to
/// grab, narrow enough to stay out of the way.
pub(crate) const HANDLE_HIT: f32 = 6.0;
/// The tint line a hovered or dragged handle draws.
const HANDLE_LINE: f32 = 2.0;
/// The document island never gets narrower than this because of the sidebar.
const MIN_DOCUMENT_WIDTH: f32 = 360.0;

/// Which resizable panel a handle belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Sidebar,
    Details,
}

/// A handle drag in progress: the panel's width and the pointer's x when the
/// button went down, so the width follows the travel instead of drifting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelDrag {
    pub(crate) panel: Panel,
    start_width: f32,
    start_x: f32,
}

/// Which way the pointer has to travel to widen a panel: the sidebar's
/// handle is on its right edge, the details panel's on its left edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrowsToward {
    Right,
    Left,
}

/// The width a panel takes while its handle is dragged from `start_x` to
/// `x`: the start width plus the travel in the growing direction, clamped to
/// `(min, max)`. A pointer position that is not a number leaves the start
/// width alone.
pub(crate) fn dragged_width(
    start_width: f32,
    start_x: f32,
    x: f32,
    grows: GrowsToward,
    (min, max): (f32, f32),
) -> f32 {
    let travel = match grows {
        GrowsToward::Right => x - start_x,
        GrowsToward::Left => start_x - x,
    };
    let width = if travel.is_finite() {
        start_width + travel
    } else {
        start_width
    };
    width.clamp(min, max.max(min))
}

/// The widest the sidebar may get in a window `window_width` wide: the
/// document island keeps [`MIN_DOCUMENT_WIDTH`] beside it.
pub(crate) fn sidebar_max_width(window_width: f32) -> f32 {
    let gutters = 3.0 * IslandMetrics::GAP.as_f32();
    (window_width - gutters - MIN_DOCUMENT_WIDTH)
        .clamp(SIDEBAR_WIDTH_RANGE.0 as f32, SIDEBAR_WIDTH_RANGE.1 as f32)
}

/// A panel's absolute bounds: the range its width is clamped to.
fn range_of(panel: Panel) -> (f32, f32) {
    let (min, max) = match panel {
        Panel::Sidebar => SIDEBAR_WIDTH_RANGE,
        Panel::Details => DETAILS_WIDTH_RANGE,
    };
    (min as f32, max as f32)
}

/// The drag handle of `panel`: a [`HANDLE_HIT`] px column with the col-resize
/// cursor and a tint line while hovered or dragged. Press starts the drag,
/// a double press restores the default width. The caller positions it.
pub(crate) fn resize_handle(
    id: &'static str,
    panel: Panel,
    dragging: bool,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let tint = ChromeColors::tint(cx.theme());
    div()
        .id(id)
        .group(id)
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(HANDLE_HIT))
        .cursor_col_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    this.reset_panel(panel, cx);
                } else {
                    this.begin_panel_drag(panel, event.position.x, window, cx);
                }
            }),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px((HANDLE_HIT - HANDLE_LINE) / 2.0))
                .w(px(HANDLE_LINE))
                .bg(if dragging {
                    tint
                } else {
                    gpui::transparent_black()
                })
                .group_hover(id, move |line| line.bg(tint)),
        )
}

impl Explorer {
    /// Starts dragging `panel`'s handle with the pointer at `x`.
    pub(crate) fn begin_panel_drag(
        &mut self,
        panel: Panel,
        x: Pixels,
        _window: &Window,
        cx: &mut Context<Self>,
    ) {
        let start_width = match panel {
            Panel::Sidebar => self.sidebar_width,
            Panel::Details => self.details_width,
        };
        self.panel_drag = Some(PanelDrag {
            panel,
            start_width,
            start_x: f32::from(x),
        });
        cx.notify();
    }

    /// The pointer moved with a drag in progress. A move without the left
    /// button held (it was released outside the window) ends the drag.
    pub(crate) fn drag_panel(
        &mut self,
        x: Pixels,
        pressed: Option<MouseButton>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.panel_drag else {
            return;
        };
        if pressed != Some(MouseButton::Left) {
            return self.end_panel_drag(cx);
        }
        let window_width = f32::from(window.viewport_size().width);
        let min = range_of(drag.panel).0;
        let (grows, max) = match drag.panel {
            Panel::Sidebar => (GrowsToward::Right, sidebar_max_width(window_width)),
            Panel::Details => (
                GrowsToward::Left,
                super::inspector::max_panel_width(window_width, self.shell_width(window)),
            ),
        };
        let width = dragged_width(
            drag.start_width,
            drag.start_x,
            f32::from(x),
            grows,
            (min, max),
        )
        .round();
        match drag.panel {
            Panel::Sidebar => self.sidebar_width = width,
            Panel::Details => self.details_width = width,
        }
        cx.notify();
    }

    /// The button was released: the width stays (it is saved with the other
    /// preferences).
    pub(crate) fn end_panel_drag(&mut self, cx: &mut Context<Self>) {
        if self.panel_drag.take().is_some() {
            cx.notify();
        }
    }

    /// A double press on the handle: the panel's default width again.
    pub(crate) fn reset_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        match panel {
            Panel::Sidebar => {
                self.sidebar_width = fileflux::core::settings::DEFAULT_SIDEBAR_WIDTH as f32;
            }
            Panel::Details => {
                self.details_width = fileflux::core::settings::DEFAULT_DETAILS_WIDTH as f32;
            }
        }
        self.panel_drag = None;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDEBAR: (f32, f32) = (200.0, 600.0);
    const DETAILS: (f32, f32) = (240.0, 600.0);

    /// Dragging the right edge of the sidebar to the right grows it by the
    /// pointer's travel, to the left shrinks it, and both ends clamp.
    #[test]
    fn dragging_a_left_panel_follows_the_pointer_and_clamps() {
        let drag = |x| dragged_width(290.0, 300.0, x, GrowsToward::Right, SIDEBAR);
        assert_eq!(drag(300.0), 290.0, "no travel, no change");
        assert_eq!(drag(350.0), 340.0);
        assert_eq!(drag(250.0), 240.0);
        assert_eq!(drag(0.0), 200.0, "never below the minimum");
        assert_eq!(drag(5000.0), 600.0, "never above the maximum");
    }

    /// The details panel's handle is on its left edge: dragging left grows it.
    #[test]
    fn dragging_a_right_panel_grows_toward_the_left_and_clamps() {
        let drag = |x| dragged_width(300.0, 900.0, x, GrowsToward::Left, DETAILS);
        assert_eq!(drag(900.0), 300.0);
        assert_eq!(drag(850.0), 350.0);
        assert_eq!(drag(950.0), 250.0);
        assert_eq!(drag(990.0), 240.0, "never below the minimum");
        assert_eq!(drag(-4000.0), 600.0, "never above the maximum");
    }

    /// The sidebar leaves the document island its minimum, and never leaves
    /// its own range.
    #[test]
    fn the_sidebar_cap_keeps_room_for_the_document() {
        assert_eq!(sidebar_max_width(4000.0), 600.0);
        assert_eq!(sidebar_max_width(900.0), 900.0 - 24.0 - MIN_DOCUMENT_WIDTH);
        assert_eq!(sidebar_max_width(100.0), 200.0, "never below the minimum");
    }

    /// A non-finite pointer position keeps the start width instead of
    /// poisoning the layout.
    #[test]
    fn a_bad_pointer_position_keeps_the_width() {
        assert_eq!(
            dragged_width(290.0, 0.0, f32::NAN, GrowsToward::Right, SIDEBAR),
            290.0
        );
    }

    /// The defaults sit inside their own ranges, so a reset is always valid.
    #[test]
    fn the_default_widths_are_inside_their_ranges() {
        let (min, max) = (SIDEBAR.0 as u32, SIDEBAR.1 as u32);
        assert!((min..=max).contains(&fileflux::core::settings::DEFAULT_SIDEBAR_WIDTH));
        let (min, max) = (DETAILS.0 as u32, DETAILS.1 as u32);
        assert!((min..=max).contains(&fileflux::core::settings::DEFAULT_DETAILS_WIDTH));
        assert_eq!(
            (SIDEBAR, DETAILS),
            (
                (
                    fileflux::core::settings::SIDEBAR_WIDTH_RANGE.0 as f32,
                    fileflux::core::settings::SIDEBAR_WIDTH_RANGE.1 as f32
                ),
                (
                    fileflux::core::settings::DETAILS_WIDTH_RANGE.0 as f32,
                    fileflux::core::settings::DETAILS_WIDTH_RANGE.1 as f32
                )
            ),
            "the drag clamps are the settings clamps"
        );
    }
}
