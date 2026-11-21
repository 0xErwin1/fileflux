//! The Fileflux explorer: the native Bolt shell, from browsing to the first file operations.

pub(crate) mod actions;
pub(crate) mod arrange;
pub(crate) mod arrange_menu;
pub(crate) mod clipboard;
pub(crate) mod devices;
pub(crate) mod dnd;
pub(crate) mod entries;
pub(crate) mod favorites;
pub(crate) mod inspector;
pub(crate) mod menu_tree;
pub(crate) mod ops;
pub(crate) mod overlay;
pub(crate) mod panes;
pub(crate) mod permissions;
pub(crate) mod persist;
pub(crate) mod progress;
pub(crate) mod rubber;
pub(crate) mod sidebar;
pub(crate) mod state;
pub(crate) mod table;
pub(crate) mod tabs;
pub(crate) mod tabstrip;
pub(crate) mod toasts;
pub(crate) mod toolbar;
pub(crate) mod typeahead;
pub(crate) mod util;
pub(crate) mod view;

pub(crate) use view::{Explorer, bind_keys};

use bolt::icon::IconSource;

/// An icon from the complete Lucide catalog the assets bundle draws, as an
/// [`IconSource`]; the curated `gpui_component::IconName` subset misses the
/// file-manager icons (Trash, House, Music, ...).
pub(crate) fn icon(name: gpui_kit_assets::IconName) -> IconSource {
    IconSource::Svg(name.path())
}
