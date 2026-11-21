//! The sidebar island: standard places, the favorites and the
//! mounted devices, discovered off the UI thread.
//!
//! Rows show the name only and navigate on click; the full path is the
//! row's tooltip and the active row shows the 2 px tint bar of the
//! reference design. Favorites are the GTK bookmarks (see
//! [`super::favorites`]), shown with their label when the bookmark has one;
//! they are added and removed for the current directory with the toolbar's
//! ⋯ menu, from a folder's context menu and from a sidebar row's menu;
//! the virtual Trash is never favorited. Devices add a 3 px usage
//! meter and a "free of" line only when `statvfs` answered; nothing is
//! invented otherwise. Every device row carries a `⋯` button at its
//! trailing edge (revealed on hover) that opens the device's menu: the
//! disk-usage analysis, a new tab, properties, and the disk's mounted
//! partitions.

use bolt::composites::{Island, ListRow};
use bolt::controls::Button;
use bolt::primitives::Icon;
use bolt::tokens::{ChromeColors, IslandMetrics, Spacing, TreeMetrics};
use fileflux::core::Location;
use fileflux::core::devices::{Device, DeviceState, device_for_mount};
use fileflux::core::places::{Volume, VolumeKind, format_bytes};
use std::path::{Path, PathBuf};

use fileflux::core::settings::{DETAILS_WIDTH_RANGE, SIDEBAR_WIDTH_RANGE};
use gpui::{
    App, Context, Div, ElementId, MouseButton, MouseDownEvent, ParentElement, Pixels, SharedString,
    Stateful, Styled, Window, div, prelude::*, px, relative,
};
use gpui_component::{ActiveTheme, tooltip::Tooltip};

use super::actions::MenuOn;
use super::devices::{actions_for, caption, unlisted_volumes};
use super::dnd::{self, DropTarget};
use super::entries;
use super::icon;
use super::ops::{MenuCommand, MenuItemSpec, MenuRow};
use super::state::Transition;
use super::tabs::Placement;
use super::view::Explorer;

/// What can open or close the parked sidebar of a narrow window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayEvent {
    /// The sidebar toggle in the toolbar.
    Toggle,
    /// Escape.
    Escape,
    /// A click anywhere outside the overlay.
    OutsideClick,
    /// A row was chosen (after navigating).
    RowChosen,
    /// The window was laid out at a width that is narrow or not.
    Resized { narrow: bool },
}

/// Whether the parked sidebar is open after `event`. It starts closed, only
/// the toggle opens it, and a window that is not narrow parks it closed
/// again, so a resize never opens it by itself.
pub(crate) fn overlay_open_after(open: bool, event: OverlayEvent) -> bool {
    match event {
        OverlayEvent::Toggle => !open,
        OverlayEvent::Escape | OverlayEvent::OutsideClick | OverlayEvent::RowChosen => false,
        OverlayEvent::Resized { narrow } => open && narrow,
    }
}

/// The sidebar island, or nothing while it is toggled closed. A wide window
/// docks it while `sidebar_open`; a narrow one overlays it only while the
/// parked overlay is open, so a collapsed sidebar must not resurrect on a
/// resize and a resize must not open the overlay.
pub(crate) fn render(
    this: &Explorer,
    narrow: bool,
    cx: &mut Context<Explorer>,
) -> Option<Stateful<Div>> {
    let shown = if narrow {
        this.sidebar_overlay_open
    } else {
        this.sidebar_open
    };
    if !shown {
        return None;
    }
    let colors = bolt::colors(cx);

    let mut island = Island::new()
        .w(px(this.sidebar_width))
        .h_full()
        .child(hamburger_row(cx))
        .child(places_section(this, cx))
        .child(favorites_section(this, cx))
        .child(network_section(this, cx))
        .child(analyzed_section(this, cx))
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
        // The parked sidebar overlays the content island from the left, in
        // the place the docked sidebar has: the overlay is positioned inside
        // the row that already ends above the status bar, so it spans that
        // row exactly and the island keeps its cut corner.
        return Some(
            div()
                .id("explorer-sidebar-overlay")
                .absolute()
                .left(IslandMetrics::GAP)
                .top_0()
                .bottom_0()
                .w(px(this.sidebar_width))
                // Every mouse event over the overlay stays on it: a click on
                // a row never reaches the toolbar or the tiles below.
                .occlude()
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

/// The Places section: Home, the user directories, Recent and the scoped
/// Trash.
/// The top of the sidebar island: the one hamburger that hides the sidebar
/// (the leftmost pane's toolbar carries the one that shows it again).
fn hamburger_row(cx: &mut Context<Explorer>) -> Stateful<Div> {
    div()
        .id("explorer-sidebar-header")
        .flex_none()
        .flex()
        .items_center()
        .h(px(44.0))
        .px(px(10.0))
        .child(
            Button::new("explorer-sidebar-hide", "Hide sidebar")
                .ghost()
                .icon_only()
                .icon(super::icon(gpui_kit_assets::IconName::Menu))
                .tooltip("Hide sidebar")
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.toggle_sidebar(window, cx);
                })),
        )
}

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
    section = section.child(nav_row(
        "place-recent".to_owned(),
        "Recent",
        this.recent_file
            .as_ref()
            .map_or_else(String::new, |file| file.to_string_lossy().into_owned()),
        gpui_kit_assets::IconName::Clock,
        &Location::Recent,
        None,
        this,
        cx,
    ));
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

/// The analysis the active tab is showing, with its state (measuring or
/// analyzed), so the sidebar points back at the root being analyzed. Absent
/// unless the active tab is a usage view: browsing elsewhere hides it.
fn analyzed_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let mut section = div().flex_none().flex().flex_col();
    let Some(session) = this.usage() else {
        return section;
    };
    let colors = bolt::colors(cx);
    let root = session.analysis_root.clone();
    let label = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let state = if session.is_walking() {
        "measuring"
    } else {
        "analyzed"
    };
    section = section
        .child(section_label("analyzed-label", "Analyzed", cx))
        .child(
            div()
                .id("explorer-analyzed-row")
                .mx(Spacing::LG)
                .h(px(26.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(Spacing::SM)
                .text_size(px(12.0))
                .text_color(colors.foreground)
                .cursor_pointer()
                .hover(|style| style.bg(colors.strong.opacity(0.06)))
                .on_click(
                    cx.listener(move |this, _event: &gpui::ClickEvent, window, cx| {
                        this.usage_drill(root.clone(), window, cx);
                    }),
                )
                .child(
                    Icon::new(super::icon(gpui_kit_assets::IconName::ChartPie))
                        .size(px(14.0))
                        .color(colors.info),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(format!("Disk usage \u{00b7} {label}")),
                )
                .child(div().flex_none().text_color(colors.muted).child(state)),
        );
    section
}

/// The Favorites section: saved rows, with an empty hint.
fn favorites_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let mut section = div().flex_none().flex().flex_col();
    section = section.child(section_label("favorites-label", "Favorites", cx));
    if this.favorites.bookmarks().is_empty() {
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
    for (index, bookmark) in this.favorites.bookmarks().iter().enumerate() {
        let path = &bookmark.path;
        section = section.child(nav_row(
            format!("favorite-{index}"),
            &bookmark.display_name(),
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

/// The Network section: "Connect to Server…", the attempt in progress (a
/// click cancels it) and the network bookmarks. Without `gio` the rows are
/// muted and say why when clicked.
fn network_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let colors = bolt::colors(cx);
    let gio = this.devices.gio.is_some();
    let mut section = div().flex_none().flex().flex_col();
    section = section.child(section_label("network-label", "Network", cx));
    section = section.child(
        network_row(
            "network-connect".to_owned(),
            ("Connect to Server\u{2026}", String::new()),
            gpui_kit_assets::IconName::Plus,
            gio,
            this,
            cx,
        )
        .on_click(cx.listener(|this, _event, window, cx| {
            this.connect_to_server(window, cx);
            this.close_sidebar_overlay(OverlayEvent::RowChosen);
        })),
    );
    if let Some(status) = this.network.status() {
        section = section.child(
            network_row(
                "network-connecting".to_owned(),
                (&status, "Click to cancel".to_owned()),
                gpui_kit_assets::IconName::X,
                true,
                this,
                cx,
            )
            .on_click(cx.listener(|this, _event, window, cx| {
                this.cancel_mount(None, window, cx);
            })),
        );
    }
    for (index, bookmark) in this.network.bookmarks.iter().enumerate() {
        let (open, menu) = (bookmark.clone(), bookmark.clone());
        let id = format!("network-bookmark-{index}");
        let menu_row = id.clone();
        section = section.child(
            network_row(
                id,
                (&bookmark.display_name(), bookmark.uri.to_string()),
                gpui_kit_assets::IconName::Network,
                gio,
                this,
                cx,
            )
            .on_click(cx.listener(move |this, _event, window, cx| {
                this.connect_to(&open.uri, open.title.as_deref(), window, cx);
                this.close_sidebar_overlay(OverlayEvent::RowChosen);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_network_menu(
                        event.position,
                        menu_row.clone(),
                        menu.clone(),
                        window,
                        cx,
                    );
                }),
            ),
        );
    }
    if !gio {
        section = section.child(
            div()
                .id("explorer-network-notice")
                .px(Spacing::LG)
                .py(Spacing::SM)
                .text_size(px(11.0))
                .line_height(px(15.0))
                .text_color(colors.muted)
                .child(super::network::MISSING_GIO),
        );
    }
    section
}

/// One row of the Network section: icon and name, muted when `enabled` is
/// false. The caller adds what a click does; a disabled row says why.
fn network_row(
    id: String,
    (name, tooltip): (&str, String),
    glyph: gpui_kit_assets::IconName,
    enabled: bool,
    this: &Explorer,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    row_shell(this, id, name.to_owned(), tooltip, (None, None), false, cx)
        .when(!enabled, |row| row.text_color(colors.muted))
        .child(
            Icon::new(icon(glyph))
                .size(TreeMetrics::ICON)
                .color(colors.muted),
        )
        .child(name.to_owned())
}

/// The Devices section: the mount table's real storage volumes with a
/// usage meter when `statvfs` answered, then the devices that are not
/// mounted (click to mount), and a one-line notice when a tool is missing.
fn volumes_section(this: &Explorer, cx: &mut Context<Explorer>) -> Div {
    let colors = bolt::colors(cx);
    let mut section = div().flex_none().flex().flex_col().pb(Spacing::MD);
    section = section.child(section_label("volumes-label", "Devices", cx));
    let Some(places) = this.places.clone() else {
        return section;
    };
    let unlisted = unlisted_volumes(&places.volumes, &this.devices.list);
    for (index, volume) in places.volumes.iter().chain(&unlisted).enumerate() {
        let device = device_for_mount(&this.devices.list, &volume.path);
        section = section.child(volume_row(
            index,
            (volume, device),
            places.home.as_path(),
            this,
            cx,
        ));
    }
    // Without `udisksctl` nothing can be mounted: those rows would only
    // tempt a click that cannot work.
    if this.devices.udisksctl.is_some() {
        let unmounted = this
            .devices
            .list
            .iter()
            .filter(|device| !matches!(device.state, DeviceState::Mounted(_)));
        for (index, device) in unmounted.enumerate() {
            section = section.child(unmounted_row(index, device, this, cx));
        }
    }
    if let Some(notice) = this.devices.notice() {
        section = section.child(
            div()
                .id("explorer-devices-notice")
                .px(Spacing::LG)
                .py(Spacing::SM)
                .text_size(px(11.0))
                .line_height(px(15.0))
                .text_color(colors.muted)
                .child(notice),
        );
    }
    section
}

/// One device that is not mounted: muted, with "Not mounted" (or "Locked",
/// or what an action is doing) under the name. A click mounts it.
fn unmounted_row(
    index: usize,
    device: &Device,
    this: &Explorer,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let glyph = if device.state == DeviceState::Locked {
        gpui_kit_assets::IconName::Lock
    } else if device.optical {
        gpui_kit_assets::IconName::Disc
    } else if device.ejectable {
        gpui_kit_assets::IconName::Usb
    } else {
        gpui_kit_assets::IconName::HardDrive
    };
    let tooltip = match &device.fstype {
        Some(fstype) => format!("{} \u{b7} {fstype}", device.path.display()),
        None => device.path.display().to_string(),
    };
    row_shell(
        this,
        format!("device-{index}"),
        device.label.clone(),
        tooltip,
        (None, Some(device.path.clone())),
        false,
        cx,
    )
    .py(Spacing::SM)
    .flex_col()
    .items_stretch()
    .gap(px(2.0))
    .text_color(colors.muted)
    .child(
        div()
            .flex()
            .items_center()
            .gap(Spacing::SM)
            .child(
                Icon::new(icon(glyph))
                    .size(TreeMetrics::ICON)
                    .color(colors.muted),
            )
            .child(device.label.clone()),
    )
    .child(
        div()
            .ml(px(25.0))
            .font_family(bolt::fonts::editor_family(cx))
            .text_size(px(11.0))
            .text_color(colors.muted)
            .child(caption(device, this.devices.working_on(device))),
    )
}

/// The icon of a device row: a phone, a share, a remote machine, a USB
/// drive or a disk.
fn volume_glyph(volume: &Volume, device: Option<&Device>) -> gpui_kit_assets::IconName {
    match volume.kind {
        VolumeKind::Phone => gpui_kit_assets::IconName::Smartphone,
        VolumeKind::Share => gpui_kit_assets::IconName::Network,
        VolumeKind::Remote => gpui_kit_assets::IconName::Server,
        VolumeKind::Disk if device.is_some_and(|device| device.optical) => {
            gpui_kit_assets::IconName::Disc
        }
        VolumeKind::Disk if volume.removable || device.is_some_and(|device| device.ejectable) => {
            gpui_kit_assets::IconName::Usb
        }
        VolumeKind::Disk => gpui_kit_assets::IconName::HardDrive,
    }
}

/// What a row's menu acts on besides its place: the block device behind a
/// disk, or the mount path of a gvfs mount (whose Unmount goes through `gio`).
fn menu_target(volume: &Volume, device: Option<&Device>) -> Option<PathBuf> {
    match device {
        Some(device) => Some(device.path.clone()),
        None if volume.kind.is_gvfs() => Some(volume.path.clone()),
        None => None,
    }
}

/// One mounted partition of a device row's disk, for the row's ⋯ menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DevicePartition {
    /// The partition's name: the label `lsblk` reported.
    pub(crate) name: String,
    /// Where it is mounted; activating the row analyzes this.
    pub(crate) mount: PathBuf,
    /// Its free space, when the matching volume's `statvfs` answered.
    pub(crate) free: Option<String>,
}

/// The mounted partitions of the disk behind `device`: every mounted
/// filesystem of it, in device-tree order, with the free space the matching
/// volume of `volumes` reported. A row without a block device (a gvfs
/// mount, or a system volume the device list leaves out) is its own single
/// partition, named as the row shows it (`own_name`).
pub(crate) fn device_partitions(
    devices: &[Device],
    volumes: &[Volume],
    own: &Volume,
    own_name: &str,
    device: Option<&Device>,
) -> Vec<DevicePartition> {
    let Some(disk) = device.map(|device| device.disk.as_path()) else {
        return vec![DevicePartition {
            name: own_name.to_owned(),
            mount: own.path.clone(),
            free: own.usage.map(|usage| format_bytes(usage.free)),
        }];
    };
    devices
        .iter()
        .filter(|device| device.disk.as_path() == disk && device.mount_point().is_some())
        .filter_map(|device| {
            let mount = device.mount_point()?.to_path_buf();
            Some(DevicePartition {
                name: device.label.clone(),
                free: volumes
                    .iter()
                    .find(|volume| volume.path == mount)
                    .and_then(|volume| volume.usage)
                    .map(|usage| format_bytes(usage.free)),
                mount,
            })
        })
        .collect()
}

/// The ⋯ menu of a sidebar device row: the device-level actions above the
/// disk's mounted partitions, each of which opens its own usage analysis
/// (see [`MenuOn::DeviceMenu`], whose custom rows these partitions fill).
pub(crate) fn device_row_menu_rows(partitions: &[DevicePartition]) -> Vec<MenuRow> {
    let mut rows = vec![
        MenuRow::Item(MenuItemSpec::new(
            MenuCommand::AnalyzeDiskUsage,
            "Analyze disk usage",
        )),
        MenuRow::Item(MenuItemSpec::new(
            MenuCommand::OpenInNewTab,
            "Open in a new tab",
        )),
        MenuRow::Item(MenuItemSpec::new(MenuCommand::OpenInSplit, "Open in split")),
        MenuRow::Item(MenuItemSpec::new(MenuCommand::Properties, "Properties")),
    ];
    if partitions.is_empty() {
        return rows;
    }
    rows.push(MenuRow::Separator);
    rows.extend(
        partitions
            .iter()
            .enumerate()
            .map(|(index, partition)| MenuRow::Custom {
                label: partition.name.clone(),
                index,
            }),
    );
    rows
}

/// One device row: icon and name, then the 3 px meter and the mono
/// "free of" line, both left out without usage numbers. The row's trailing
/// `⋯` opens the device's menu (revealed on hover, held open while the
/// menu is); the eject button, where the device has one, keeps its place
/// before it.
fn volume_row(
    index: usize,
    (volume, device): (&Volume, Option<&Device>),
    home: &Path,
    this: &Explorer,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let location = Location::Directory(volume.path.clone());
    let active = this.tab().state.current() == &location;
    let glyph = volume_glyph(volume, device);
    let row_id = format!("volume-{index}");
    let name = if volume.path == home {
        "Home".to_owned()
    } else {
        volume.label.clone()
    };
    let name_for_menu = name.clone();
    let actions = device.map(|device| {
        actions_for(
            device,
            this.devices.udisksctl.is_some(),
            this.devices.gio.is_some(),
        )
    });
    let mut top = div()
        .flex()
        .items_center()
        .gap(Spacing::SM)
        .child(
            Icon::new(icon(glyph))
                .size(TreeMetrics::ICON)
                .color(if active { tint } else { colors.muted }),
        )
        .child(name);
    // The eject button of the reference design: right-aligned on removable
    // rows, "Eject <name>" on hover.
    let mut has_eject = false;
    if let (Some(device), Some(actions)) = (device, actions)
        && actions.eject
    {
        has_eject = true;
        let target = device.path.clone();
        let tooltip = format!("Eject {}", device.label);
        top = top.child(
            div()
                .id(("volume-eject", index))
                .ml_auto()
                .flex_none()
                .size(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.muted)
                .hover(|button| button.bg(colors.line).text_color(colors.strong))
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.eject_device(&target, window, cx);
                }))
                .child(Icon::new(icon(gpui_kit_assets::IconName::Eject)).size(px(13.0))),
        );
    }
    // The ⋯ of the device menu: revealed while the row is hovered or its
    // menu is open, so the name keeps its room.
    let menu_open = matches!(
        this.menu.as_ref().map(|menu| &menu.on),
        Some(MenuOn::DeviceMenu { row, .. }) if *row == row_id
    );
    let group: SharedString = format!("explorer-volume-group-{index}").into();
    let hover_group = group.clone();
    let dots = (
        volume.clone(),
        name_for_menu.clone(),
        device.map(|device| device.path.clone()),
    );
    let dots_row = row_id.clone();
    top = top.child(
        div()
            .id(("volume-dots", index))
            .flex_none()
            .when(!has_eject, |dots| dots.ml_auto())
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(colors.muted)
            .when(!menu_open, |dots| {
                dots.invisible()
                    .hover(|dots| dots.bg(colors.line).text_color(colors.strong))
                    .group_hover(hover_group.clone(), |dots| dots.visible())
            })
            .when(menu_open, |dots| {
                dots.border_1()
                    .border_color(colors.line_2)
                    .bg(colors.raised)
                    .text_color(colors.foreground)
            })
            .tooltip(|window, cx| {
                Tooltip::new(
                    "Analyze disk usage, open in a new tab, properties and the disk's partitions",
                )
                .build(window, cx)
            })
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    cx.stop_propagation();
                    let position = super::overlay::under_button(event.position());
                    let (volume, name, device) = dots.clone();
                    this.open_device_row_menu(
                        position,
                        dots_row.clone(),
                        volume,
                        name,
                        device,
                        window,
                        cx,
                    );
                }),
            )
            .child(Icon::new(icon(gpui_kit_assets::IconName::Ellipsis)).size(px(13.0))),
    );
    let mut row = row_shell(
        this,
        row_id,
        name_for_menu,
        volume.path.to_string_lossy().into_owned(),
        (Some(location), menu_target(volume, device)),
        active,
        cx,
    )
    .group(group)
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
///
/// `target` is the place the row stands for and the block device behind it,
/// when it is one. A row without a place is a device that is not mounted: a
/// click mounts it, and nothing can be dropped on it.
fn row_shell(
    this: &Explorer,
    id: String,
    name: String,
    path: String,
    (location, device): (Option<Location>, Option<PathBuf>),
    active: bool,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    // The row its context menu is open on keeps the tint outline.
    let outlined = matches!(
        this.menu.as_ref().map(|menu| &menu.on),
        Some(
            MenuOn::Sidebar { row, .. }
                | MenuOn::Network { row, .. }
                | MenuOn::DeviceMenu { row, .. }
        ) if *row == id
    );
    let menu_row = id.clone();
    let menu_target = (location.clone(), device.clone());
    let click_target = (location.clone(), device.clone());
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
        .on_click(
            cx.listener(move |this, _event, window, cx| match &click_target {
                (Some(location), _) => {
                    this.navigate(Transition::Visit(location.clone()), window, cx);
                    this.close_sidebar_overlay(OverlayEvent::RowChosen);
                }
                (None, Some(device)) => {
                    this.activate_device(device.clone(), window, cx);
                    this.close_sidebar_overlay(OverlayEvent::RowChosen);
                }
                (None, None) => {}
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                match &menu_target {
                    (Some(location), device) => this.open_sidebar_menu(
                        event.position,
                        menu_row.clone(),
                        name.clone(),
                        (location.clone(), device.clone()),
                        window,
                        cx,
                    ),
                    (None, Some(device)) => this.open_device_menu(
                        event.position,
                        menu_row.clone(),
                        name.clone(),
                        device.clone(),
                        window,
                        cx,
                    ),
                    (None, None) => {}
                }
            }),
        );
    let row = match &location {
        Some(location) => {
            let middle = location.clone();
            let row = row.on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _event, window, cx| {
                    this.open_tab(middle.clone(), Placement::Background, window, cx);
                }),
            );
            // Dropping on a row is dropping on its place: a folder, or the
            // Trash.
            let target = match location {
                Location::Directory(path) => Some(DropTarget::Folder(path.clone())),
                Location::Trash => Some(DropTarget::Trash),
                // Nothing is dropped into the list of recent files.
                Location::Recent | Location::Search { .. } | Location::Usage { .. } => None,
            };
            match target {
                Some(target) => dnd::drop_target(row, target, &cx.entity().downgrade(), cx),
                None => row,
            }
        }
        None => row,
    };
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
        (Some(location.clone()), None),
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

    /// The parked sidebar opens only through the toggle, closes on Escape,
    /// an outside click or a chosen row, and a resize never opens it.
    #[test]
    fn the_parked_overlay_opens_only_by_the_toggle() {
        use OverlayEvent::*;
        assert!(overlay_open_after(false, Toggle));
        assert!(!overlay_open_after(true, Toggle));
        for closing in [Escape, OutsideClick, RowChosen] {
            assert!(!overlay_open_after(true, closing), "{closing:?} closes");
            assert!(
                !overlay_open_after(false, closing),
                "{closing:?} stays closed"
            );
        }
        assert!(
            !overlay_open_after(false, Resized { narrow: true }),
            "becoming narrow does not open it"
        );
        assert!(overlay_open_after(true, Resized { narrow: true }));
        assert!(
            !overlay_open_after(true, Resized { narrow: false }),
            "a wide window parks it closed"
        );
    }
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
    fn volume(kind: VolumeKind, removable: bool) -> Volume {
        Volume {
            label: "x".to_owned(),
            path: PathBuf::from("/x"),
            removable,
            usage: None,
            kind,
        }
    }

    #[test]
    fn each_kind_of_volume_has_its_own_icon() {
        use gpui_kit_assets::IconName;
        let glyph = |kind, removable| volume_glyph(&volume(kind, removable), None);
        assert_eq!(glyph(VolumeKind::Phone, false), IconName::Smartphone);
        assert_eq!(glyph(VolumeKind::Share, false), IconName::Network);
        assert_eq!(glyph(VolumeKind::Remote, false), IconName::Server);
        assert_eq!(glyph(VolumeKind::Disk, true), IconName::Usb);
        assert_eq!(glyph(VolumeKind::Disk, false), IconName::HardDrive);
    }

    #[test]
    fn a_disc_has_the_disc_icon_whatever_the_removable_flag() {
        let disc = Device {
            path: PathBuf::from("/dev/sr0"),
            disk: PathBuf::from("/dev/sr0"),
            label: "DVD".to_owned(),
            fstype: Some("iso9660".to_owned()),
            size: 1,
            state: DeviceState::Unmounted,
            mounts: Vec::new(),
            ejectable: true,
            container: None,
            optical: true,
        };
        for removable in [true, false] {
            assert_eq!(
                volume_glyph(&volume(VolumeKind::Disk, removable), Some(&disc)),
                gpui_kit_assets::IconName::Disc
            );
        }
        let usb = Device {
            optical: false,
            ..disc
        };
        assert_eq!(
            volume_glyph(&volume(VolumeKind::Disk, false), Some(&usb)),
            gpui_kit_assets::IconName::Usb
        );
    }

    #[test]
    fn only_a_gvfs_row_without_a_block_device_acts_on_its_own_path() {
        let phone = volume(VolumeKind::Phone, false);
        assert_eq!(menu_target(&phone, None), Some(PathBuf::from("/x")));
        assert_eq!(menu_target(&volume(VolumeKind::Disk, false), None), None);
    }

    fn partition_device(path: &str, disk: &str, label: &str, mount: Option<&str>) -> Device {
        Device {
            path: PathBuf::from(path),
            disk: PathBuf::from(disk),
            label: label.to_owned(),
            fstype: Some("ext4".to_owned()),
            size: 0,
            state: match mount {
                Some(mount) => DeviceState::Mounted(PathBuf::from(mount)),
                None => DeviceState::Unmounted,
            },
            mounts: mount
                .map(|mount| vec![PathBuf::from(mount)])
                .unwrap_or_default(),
            ejectable: false,
            container: None,
            optical: false,
        }
    }

    fn usage_volume(path: &str, label: &str, free: Option<u64>) -> Volume {
        Volume {
            label: label.to_owned(),
            path: PathBuf::from(path),
            removable: false,
            usage: free.map(|free| fileflux::core::places::Usage {
                total: free * 2,
                free,
            }),
            kind: VolumeKind::Disk,
        }
    }

    /// A device row's ⋯ menu lists the mounted partitions of its disk, with
    /// the free space the matching volume reported; the unmounted siblings
    /// and other disks' partitions stay out.
    #[test]
    fn a_device_row_lists_the_mounted_partitions_of_its_disk() {
        let devices = vec![
            partition_device("/dev/sdb1", "/dev/sdb", "vault1", Some("/home")),
            partition_device("/dev/sdb2", "/dev/sdb", "vault2", Some("/")),
            partition_device("/dev/sdb3", "/dev/sdb", "vault3", None),
            partition_device("/dev/sdc1", "/dev/sdc", "other", Some("/mnt/c")),
        ];
        let volumes = vec![
            usage_volume("/home", "home", Some(186)),
            usage_volume("/", "System", None),
        ];
        let own = usage_volume("/home", "home", Some(186));
        assert_eq!(
            device_partitions(&devices, &volumes, &own, "Home", devices.first()),
            vec![
                DevicePartition {
                    name: "vault1".to_owned(),
                    mount: PathBuf::from("/home"),
                    free: Some("186 B".to_owned()),
                },
                DevicePartition {
                    name: "vault2".to_owned(),
                    mount: PathBuf::from("/"),
                    // A volume statvfs could not be read invents no number.
                    free: None,
                },
            ]
        );
    }

    /// A row without a block device (a gvfs mount, or a system volume the
    /// device list leaves out) is its own single partition, named as the
    /// row shows it.
    #[test]
    fn a_row_without_a_block_device_is_its_own_partition() {
        let own = Volume {
            label: "Pixel".to_owned(),
            path: PathBuf::from("/run/user/1000/gvfs/mtp:host=x"),
            removable: false,
            usage: Some(fileflux::core::places::Usage {
                total: 64,
                free: 12,
            }),
            kind: VolumeKind::Phone,
        };
        assert_eq!(
            device_partitions(&[], &[], &own, "Pixel", None),
            vec![DevicePartition {
                name: "Pixel".to_owned(),
                mount: own.path.clone(),
                free: Some("12 B".to_owned()),
            }]
        );
    }

    /// The ⋯ menu holds the device-level actions above the partitions; a
    /// partition is the custom row whose index the activation reads.
    #[test]
    fn the_device_row_menu_holds_the_device_actions_then_its_partitions() {
        let partitions = [DevicePartition {
            name: "vault1".to_owned(),
            mount: PathBuf::from("/home"),
            free: Some("186 B".to_owned()),
        }];
        let rows = device_row_menu_rows(&partitions);
        let labels: Vec<String> = rows
            .iter()
            .map(|row| match row {
                MenuRow::Separator => "-".to_owned(),
                MenuRow::Item(spec) => spec.label.to_owned(),
                MenuRow::Custom { label, .. } => label.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Analyze disk usage",
                "Open in a new tab",
                "Open in split",
                "Properties",
                "-",
                "vault1"
            ]
        );
        assert!(matches!(
            rows.last(),
            Some(MenuRow::Custom {
                label,
                index: 0
            }) if label == "vault1"
        ));
        // Without partitions there is no separator either.
        assert_eq!(device_row_menu_rows(&[]).len(), 4);
    }
}
