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
        // Bolt derives the light text accent from its own field now; Fileflux
        // keeps the cobalt accent doubling as the light text accent, which is
        // how Bolt's light palette resolved `tint` before the field existed.
        accent_text_light: hex(0x2B45B8),
        // Bolt derives syntax keywords from their own fields now instead of
        // from the accent: dark surfaces keep the accent tint (0x9DB1FF) and
        // light surfaces the accent (0x2B45B8), the colors the previous
        // derivation gave them.
        keyword_dark: hex(0x9DB1FF),
        keyword_light: hex(0x2B45B8),
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

    /// Bolt derives the light text accent and the syntax keywords from the brand
    /// now, where it used the accent before. Fileflux's brand keeps those roles
    /// equal to the colors the old derivation gave them, so the explorer renders
    /// exactly as it did.
    #[test]
    fn the_brand_keeps_the_colors_bolt_used_to_derive() {
        let brand = brand();
        assert_eq!(
            brand.accent_text_light, brand.accent,
            "the light text accent was the accent itself"
        );
        assert_eq!(
            brand.keyword_dark, brand.accent_tint,
            "a keyword on dark surfaces was the accent tint"
        );
        assert_eq!(
            brand.keyword_light, brand.accent,
            "a keyword on light surfaces was the accent itself"
        );
    }
}
