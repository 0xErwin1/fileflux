//! The sidebar island: standard places, the session favorites and the
//! mounted devices, discovered off the UI thread.
//!
//! Rows show the name only and navigate on click; the full path is the
//! row's tooltip and the active row shows the 2 px tint bar of the
//! reference design. Favorites are session-only (nothing is written) and
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

use gpui::{
    App, Context, Div, ElementId, ParentElement, Stateful, Styled, div, prelude::*, px, relative,
};
use gpui_component::{ActiveTheme, tooltip::Tooltip};

use super::icon;
use super::state::Transition;
use super::view::{Explorer, SIDEBAR_WIDTH};

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
        .w(px(SIDEBAR_WIDTH))
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
                .w(px(SIDEBAR_WIDTH))
                .overflow_y_scroll()
                .child(island),
        );
    }

    Some(
        div()
            .id("explorer-sidebar-slot")
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
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

/// The Favorites section: session-only rows, with an empty hint.
fn favorites_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let mut section = div().flex_none().flex().flex_col();
    section = section.child(section_label("favorites-label", "Favorites", cx));
    if this.state.favorites().is_empty() {
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
    for (index, path) in this.state.favorites().iter().enumerate() {
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
    let active = this.state.current() == &location;
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
        format!("volume-{index}"),
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
    id: String,
    path: String,
    location: Location,
    active: bool,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let row = ListRow::new(ElementId::Name(id.into()))
        .selected(active)
        .selection_bar(true)
        .build(cx)
        .flex_none()
        .min_h(px(26.0))
        .flex()
        .items_center()
        .gap(Spacing::SM)
        .px(Spacing::LG)
        .text_size(px(13.0))
        .text_color(if active {
            colors.strong
        } else {
            colors.foreground
        })
        .on_click(cx.listener(move |this, _event, window, cx| {
            this.navigate(Transition::Visit(location.clone()), window, cx);
        }));
    if path.is_empty() {
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
    let active = this.state.current() == location;
    let home_label = home.is_some_and(|home| location == &Location::Directory(home.to_path_buf()));
    let shown_label = if home_label {
        "Home".to_owned()
    } else {
        label.to_owned()
    };
    row_shell(id, path, location.clone(), active, cx)
        .child(
            Icon::new(icon(glyph))
                .size(TreeMetrics::ICON)
                .color(if active { tint } else { colors.muted }),
        )
        .child(shown_label)
}

// The favorites rows show their own file names; no home rewriting, so
// their `home` comparison input stays unset.
