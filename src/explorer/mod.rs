//! The Fileflux explorer: the native Bolt shell for the read-only MVP.

pub(crate) mod entries;
pub(crate) mod inspector;
pub(crate) mod sidebar;
pub(crate) mod state;
pub(crate) mod table;
pub(crate) mod toolbar;
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
