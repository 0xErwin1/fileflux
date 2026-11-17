//! The sidebar island: standard places, the session favorites and the
//! mounted volumes, discovered off the UI thread.
//!
//! Rows navigate on click; the active row shows the 2 px tint bar of the
//! reference design. Favorites are session-only (nothing is written) and
//! are added and removed for the current directory with the toolbar's star
//! button; the virtual Trash is never favorited. Volumes show their exact
//! mount paths and nothing invented: no free-space numbers.

use bolt::composites::{Island, ListRow};
use bolt::primitives::Icon;
use bolt::tokens::{ChromeColors, IslandMetrics, Spacing, TreeMetrics};
use fileflux::core::Location;
use gpui::{App, Context, Div, ElementId, ParentElement, Stateful, Styled, div, prelude::*, px};
use gpui_component::ActiveTheme;

use super::icon;
use super::state::Transition;
use super::view::{Explorer, SIDEBAR_WIDTH};

/// The sidebar island, or nothing while a narrow window keeps it parked.
pub(crate) fn render(
    this: &Explorer,
    narrow: bool,
    cx: &mut Context<Explorer>,
) -> Option<Stateful<Div>> {
    if narrow && !this.sidebar_open {
        return None;
    }
    let colors = bolt::colors(cx);

    let mut island = Island::new()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .child(header(cx))
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

    let island = island.child(
        div()
            .flex_none()
            .min_h(px(46.0))
            .p(Spacing::LG)
            .text_size(px(12.0))
            .text_color(colors.muted)
            .child("Local files, read-only"),
    );

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

/// The `Explorer` header row of the sidebar island.
fn header(cx: &App) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    div()
        .id("explorer-sidebar-header")
        .flex_none()
        .min_h(px(46.0))
        .flex()
        .items_center()
        .gap(Spacing::SM)
        .px(Spacing::LG)
        .border_b_1()
        .border_color(colors.line)
        .text_color(colors.strong)
        .child(
            Icon::new(icon(gpui_kit_assets::IconName::FolderOpen))
                .color(ChromeColors::tint(cx.theme())),
        )
        .child("Explorer")
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
    section = section.child(section_label(
        "favorites-label",
        "Favorites (session only)",
        cx,
    ));
    if this.state.favorites().is_empty() {
        return section.child(
            div()
                .id("explorer-favorites-empty")
                .px(Spacing::LG)
                .pb(Spacing::SM)
                .text_size(px(12.0))
                .text_color(bolt::colors(cx).muted)
                .child("No favorites yet — use the star in the toolbar."),
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

/// The Volumes section: the mount table's real storage volumes, at their
/// exact mount paths.
fn volumes_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let mut section = div().flex_none().flex().flex_col().pb(Spacing::MD);
    section = section.child(section_label("volumes-label", "Volumes", cx));
    let Some(places) = this.places.clone() else {
        return section;
    };
    for (index, volume) in places.volumes.iter().enumerate() {
        section = section.child(nav_row(
            format!("volume-{index}"),
            &volume.label,
            volume.path.to_string_lossy().into_owned(),
            gpui_kit_assets::IconName::HardDrive,
            &Location::Directory(volume.path.clone()),
            Some(places.home.as_path()),
            this,
            cx,
        ));
    }
    section
}

/// One sidebar navigation row: icon, label, the exact path as a truncated
/// subtitle, and the active treatment for the current location.
#[allow(clippy::too_many_arguments)]
fn nav_row(
    id: String,
    label: &str,
    subtitle: String,
    glyph: gpui_kit_assets::IconName,
    location: &Location,
    home: Option<&std::path::Path>,
    this: &Explorer,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let source = icon(glyph);
    let active = this.state.current() == location;
    let home_label = home.is_some_and(|home| location == &Location::Directory(home.to_path_buf()));
    let shown_label = if home_label {
        "Home".to_owned()
    } else {
        label.to_owned()
    };
    let target = location.clone();
    ListRow::new(ElementId::Name(id.into()))
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
            this.navigate(Transition::Visit(target.clone()), window, cx);
        }))
        .child(Icon::new(source).size(TreeMetrics::ICON).color(if active {
            tint
        } else {
            colors.muted
        }))
        .child(shown_label)
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(px(10.5))
                .text_color(colors.muted)
                .child(subtitle),
        )
}

// The favorites rows show their own file names; no home rewriting, so
// their `home` comparison input stays unset.
