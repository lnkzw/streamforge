//! The bundled typefaces (IBM Plex, SIL OFL 1.1; `assets/fonts`), so the
//! interface looks the same on every system and never depends on which
//! fonts are installed.

use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

const SANS_REGULAR: &[u8] = include_bytes!("../../assets/fonts/IBMPlexSans-Regular.ttf");
const SANS_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/IBMPlexSans-Medium.ttf");
const SANS_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/IBMPlexSans-SemiBold.ttf");
const MONO_REGULAR: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf");
const MONO_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Medium.ttf");

/// Font family names for the weights egui's two built-in families do not
/// cover; see [`super::theme::TextRole`].
pub const SANS_MEDIUM_FAMILY: &str = "sans-medium";
pub const SANS_SEMIBOLD_FAMILY: &str = "sans-semibold";
pub const MONO_MEDIUM_FAMILY: &str = "mono-medium";

/// Install the fonts: Plex Sans Regular is the proportional face, Plex Mono
/// Regular the monospace one, and the medium and semibold weights have
/// their own families. egui's default fonts stay behind them as fallback
/// for glyphs Plex lacks.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let defaults_proportional = fonts.families[&FontFamily::Proportional].clone();
    let defaults_monospace = fonts.families[&FontFamily::Monospace].clone();
    for (name, bytes) in [
        ("plex-sans-regular", SANS_REGULAR),
        ("plex-sans-medium", SANS_MEDIUM),
        ("plex-sans-semibold", SANS_SEMIBOLD),
        ("plex-mono-regular", MONO_REGULAR),
        ("plex-mono-medium", MONO_MEDIUM),
    ] {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    let family = |first: &str, fallback: &[String]| {
        std::iter::once(first.to_owned())
            .chain(fallback.iter().cloned())
            .collect::<Vec<_>>()
    };
    fonts.families.insert(
        FontFamily::Proportional,
        family("plex-sans-regular", &defaults_proportional),
    );
    fonts.families.insert(
        FontFamily::Monospace,
        family("plex-mono-regular", &defaults_monospace),
    );
    fonts.families.insert(
        FontFamily::Name(SANS_MEDIUM_FAMILY.into()),
        family("plex-sans-medium", &defaults_proportional),
    );
    fonts.families.insert(
        FontFamily::Name(SANS_SEMIBOLD_FAMILY.into()),
        family("plex-sans-semibold", &defaults_proportional),
    );
    fonts.families.insert(
        FontFamily::Name(MONO_MEDIUM_FAMILY.into()),
        family("plex-mono-medium", &defaults_monospace),
    );
    ctx.set_fonts(fonts);
}
