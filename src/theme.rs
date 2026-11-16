//! Fileflux's Bolt installation: the provisional brand and the startup theme.

use bolt::brand::Brand;
use bolt::theme::rgb_to_hsla as hex;
use gpui::App;

/// The brand Fileflux installs into `bolt`: ReqFlux's cobalt accent, renamed
/// until the family settles on Fileflux's own colors.
pub(crate) fn brand() -> Brand {
    Brand {
        name: "Fileflux".into(),
        accent: hex(0x2B45B8),
        accent_hover: hex(0x3451C8),
        accent_pressed: hex(0x1F3389),
        accent_tint: hex(0x9DB1FF),
        on_accent: hex(0xFFFFFF),
    }
}

/// Initializes `gpui-component`, loads Bolt's bundled fonts at the default
/// interface size, the default density, and installs the Bolt theme for
/// `mode` with Fileflux's brand. [`PaletteMode::System`] resolves against the
/// platform appearance when the theme applies.
pub(crate) fn install(mode: bolt::palette::PaletteMode, cx: &mut App) {
    if !cx.has_global::<gpui_component::theme::Theme>() {
        gpui_component::init(cx);
    }
    bolt::typography::load_bundled_fonts(cx);
    bolt::fonts::init(cx, bolt::fonts::FontSettings::default());
    bolt::density::init(cx, bolt::density::Density::Default);
    bolt::theme::install(brand(), mode, bolt::density::Density::Default, cx);
}
