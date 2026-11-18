//! Fileflux's Bolt installation: the provisional brand and the startup theme.

use bolt::brand::Brand;
use bolt::theme::rgb_to_hsla as hex;
use gpui::App;

/// The palette Fileflux starts with. The chosen design is dark, so the
/// explorer always opens with Bolt's existing dark palette, regardless of
/// the platform appearance.
pub(crate) const STARTUP_PALETTE: bolt::palette::PaletteMode = bolt::palette::PaletteMode::Dark;

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
/// `mode` with Fileflux's brand.
pub(crate) fn install(mode: bolt::palette::PaletteMode, cx: &mut App) {
    if !cx.has_global::<gpui_component::theme::Theme>() {
        gpui_component::init(cx);
    }
    bolt::typography::load_bundled_fonts(cx);
    bolt::fonts::init(cx, bolt::fonts::FontSettings::default());
    bolt::density::init(cx, bolt::density::Density::Default);
    bolt::theme::install(brand(), mode, bolt::density::Density::Default, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The explorer must start dark no matter how the OS appears, so the
    /// startup selection can never drift back to `System` (or `Light`).
    #[test]
    fn startup_palette_is_dark() {
        assert_eq!(
            STARTUP_PALETTE,
            bolt::palette::PaletteMode::Dark,
            "Fileflux must start with Bolt's dark palette, independent of the OS appearance"
        );
    }
}
