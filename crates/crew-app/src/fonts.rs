//! Geist and Geist Mono, embedded, with system fallbacks.

use std::borrow::Cow;
use std::sync::OnceLock;

use gpui_kit::{App, SharedString};

/// Geist and Geist Mono (SIL OFL 1.1, `assets/fonts/OFL.txt`), one file per
/// weight and style the app draws.
const EMBEDDED_FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Geist-Regular.ttf"),
    include_bytes!("../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Geist-Italic.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
    include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf"),
];

const FALLBACK_UI: &str = ".SystemUIFont";
const FALLBACK_MONO: &str = "Menlo";

static FONTS: OnceLock<(SharedString, SharedString)> = OnceLock::new();

/// Registers the embedded fonts and picks the default families.
pub fn load(cx: &App) {
    let fonts = EMBEDDED_FONTS.iter().map(|&b| Cow::Borrowed(b)).collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        eprintln!("crew: embedded fonts did not load: {e}");
    }
    let (ui, mono) = families(&cx.text_system().all_font_names());
    let _ = FONTS.set((ui.into(), mono.into()));
}

/// The default interface font family: Geist, or the system font without
/// it. Views use `theme::scale::ui_family`, which follows the settings.
pub fn ui_font() -> SharedString {
    FONTS.get().map_or(FALLBACK_UI.into(), |f| f.0.clone())
}

/// The default code font family: Geist Mono, or Menlo without it.
pub fn mono_font() -> SharedString {
    FONTS.get().map_or(FALLBACK_MONO.into(), |f| f.1.clone())
}

/// Geist and Geist Mono if installed, else the system UI font and Menlo.
/// GPUI panics on a family it can't find, so never name a missing one.
fn families(installed: &[String]) -> (&'static str, &'static str) {
    let has = |family: &str| installed.iter().any(|f| f == family);
    (
        if has("Geist") { "Geist" } else { FALLBACK_UI },
        if has("Geist Mono") {
            "Geist Mono"
        } else {
            FALLBACK_MONO
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{EMBEDDED_FONTS, families};

    #[test]
    fn fonts_fall_back_when_geist_is_missing() {
        let installed = ["Helvetica".to_string(), "Menlo".to_string()];
        assert_eq!(families(&installed), (".SystemUIFont", "Menlo"));
    }

    #[test]
    fn fonts_prefer_geist_when_installed() {
        let installed = ["Geist".to_string(), "Geist Mono".to_string()];
        assert_eq!(families(&installed), ("Geist", "Geist Mono"));
    }

    #[test]
    fn embedded_fonts_are_truetype_with_their_license() {
        for font in EMBEDDED_FONTS {
            assert_eq!(&font[..4], &[0, 1, 0, 0], "not a TrueType file");
        }
        let license = include_str!("../assets/fonts/OFL.txt");
        assert!(license.contains("SIL Open Font License, Version 1.1"));
    }
}
