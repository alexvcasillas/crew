//! Sizes, weights and fonts from the appearance settings. Every size in
//! the app goes through these helpers: layout lengths follow the zoom, text
//! follows its size setting and the zoom, weights follow Bolder text.
//!
//! The helpers read a per-thread copy of the settings, so views call them
//! without a context. GPUI draws on one thread; tests each get their own.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::component::{Theme, ThemeConfig};
use gpui_kit::{
    App, DefiniteLength, FontWeight, Global, Pixels, SharedString, Task, phi, px, relative,
};

use crate::fonts;
use crate::prefs::{self, Appearance, DEFAULT_CODE_FONT, DEFAULT_UI_FONT};

/// The interface text size the app was drawn at, which `text` scales from.
const BASE_UI: f32 = 13.;
/// The code text size the app was drawn at, which `mono` scales from.
const BASE_CODE: f32 = 12.;
/// The Kit's base size at the original look: its rem.
const KIT_BASE: f32 = 13.;

impl Global for Appearance {}

/// The factors the helpers apply, worked out once per change.
#[derive(Debug, Clone, PartialEq)]
struct Scale {
    zoom: f32,
    text: f32,
    code: f32,
    line: f32,
    row: f32,
    bolder: bool,
    ui_family: Option<SharedString>,
    code_family: Option<SharedString>,
}

impl Default for Scale {
    fn default() -> Self {
        Self::of(&Appearance::default(), None, None)
    }
}

impl Scale {
    fn of(
        a: &Appearance,
        ui_family: Option<SharedString>,
        code_family: Option<SharedString>,
    ) -> Self {
        let zoom = f32::from(a.zoom) / 100.;
        let text = f32::from(a.ui_size) / BASE_UI;
        let default_line = prefs::LineHeight::Comfortable.multiplier();
        Self {
            zoom,
            text,
            code: f32::from(a.code_size) / BASE_CODE,
            line: a.line_height.multiplier() / default_line,
            // Rows grow with bigger text so the text still fits, and never
            // shrink below their original height for smaller text.
            row: a.density.factor() * text.max(1.),
            bolder: a.bolder,
            ui_family,
            code_family,
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Scale> = RefCell::new(Scale::default());
}

fn with<R>(f: impl FnOnce(&Scale) -> R) -> R {
    CURRENT.with(|s| f(&s.borrow()))
}

fn set_scale(scale: Scale) {
    CURRENT.with(|s| *s.borrow_mut() = scale);
}

/// A layout length: `n` px at 100% zoom.
pub fn ui_px(n: f32) -> Pixels {
    px(n * with(|s| s.zoom))
}

/// Interface text: `n` px at the default interface size.
pub fn text(n: f32) -> Pixels {
    px(n * with(|s| s.text * s.zoom))
}

/// Code text: `n` px at the default code size.
pub fn mono(n: f32) -> Pixels {
    px(n * with(|s| s.code * s.zoom))
}

/// The height of a line of interface text that is `n` px at the default
/// size and line height.
pub fn line(n: f32) -> Pixels {
    px(n * with(|s| s.line * s.text * s.zoom))
}

/// The height of a line of code that is `n` px at the default size and
/// line height.
pub fn mono_line(n: f32) -> Pixels {
    px(n * with(|s| s.line * s.code * s.zoom))
}

/// A row height or a row's vertical padding: `n` px at the default
/// density, text size and zoom. Whole pixels, so row borders stay sharp.
pub fn row(n: f32) -> Pixels {
    px((n * with(|s| s.row * s.zoom)).round())
}

/// `w`, one step heavier with Bolder text, up to Black.
pub fn weight(w: FontWeight) -> FontWeight {
    if with(|s| s.bolder) {
        FontWeight((w.0 + 100.).min(FontWeight::BLACK.0))
    } else {
        w
    }
}

/// The line height text without one of its own gets, relative to its
/// size: GPUI's default at the default line height.
pub fn relative_line() -> DefiniteLength {
    match phi() {
        DefiniteLength::Fraction(f) => relative(f * with(|s| s.line)),
        other => other,
    }
}

/// The interface font family in use.
pub fn ui_family() -> SharedString {
    with(|s| s.ui_family.clone()).unwrap_or_else(fonts::ui_font)
}

/// The code font family in use.
pub fn code_family() -> SharedString {
    with(|s| s.code_family.clone()).unwrap_or_else(fonts::mono_font)
}

/// The Kit's base text size, its rem: buttons, inputs, selects, menus and
/// tooltips size themselves from it.
fn kit_font_size() -> Pixels {
    text(KIT_BASE)
}

/// The Kit's code text size.
fn kit_mono_size() -> Pixels {
    mono(KIT_BASE)
}

/// `chosen` if it is installed, else the slot's default family. GPUI
/// panics on a family it can't find, so never name a missing one.
fn resolve_family(
    chosen: &str,
    default_name: &str,
    installed: impl FnOnce(&str) -> bool,
) -> Option<SharedString> {
    if chosen == default_name || !installed(chosen) {
        None
    } else {
        Some(SharedString::from(chosen.to_string()))
    }
}

/// Where the settings are saved, and the save in flight.
struct Store {
    home: PathBuf,
    /// Numbers the saves, so an older one never lands over a newer.
    seq: Arc<AtomicU64>,
    _save: Option<Task<()>>,
}

impl Global for Store {}

/// Reads the settings saved in `crew_home` and applies them. Later
/// changes are saved there.
pub fn load(crew_home: PathBuf, cx: &mut App) {
    let appearance = Appearance::load(&crew_home);
    cx.set_global(Store {
        home: crew_home,
        seq: Arc::new(AtomicU64::new(0)),
        _save: None,
    });
    apply(appearance, cx);
}

/// The settings in use.
pub fn appearance(cx: &App) -> Appearance {
    cx.try_global::<Appearance>().cloned().unwrap_or_default()
}

/// Applies `appearance` to every window and saves it.
pub fn set_appearance(appearance: Appearance, cx: &mut App) {
    apply(appearance.clone(), cx);
    let Some(store) = cx.try_global::<Store>() else {
        return;
    };
    let home = store.home.clone();
    let seq = store.seq.clone();
    let mine = seq.fetch_add(1, Ordering::SeqCst) + 1;
    let save = cx.background_executor().spawn(async move {
        if seq.load(Ordering::SeqCst) == mine
            && let Err(e) = appearance.save(&home)
        {
            eprintln!("crew: appearance not saved: {e}");
        }
    });
    cx.global_mut::<Store>()._save = Some(save);
}

/// Changes the zoom by one step: 1 bigger, -1 smaller, 0 back to 100%.
pub fn step_zoom(direction: i8, cx: &mut App) {
    let mut a = appearance(cx);
    a.zoom = if direction == 0 {
        100
    } else {
        prefs::step_zoom(a.zoom, direction)
    };
    set_appearance(a, cx);
}

/// Makes `appearance` the one the helpers and the Kit read, and redraws.
fn apply(appearance: Appearance, cx: &mut App) {
    // Listing the installed fonts is slow, so only for a font of your own.
    let mut names: Option<Vec<String>> = None;
    let mut installed = |family: &str| {
        names
            .get_or_insert_with(|| cx.text_system().all_font_names())
            .iter()
            .any(|f| f == family)
    };
    let ui = resolve_family(&appearance.ui_font, DEFAULT_UI_FONT, &mut installed);
    let code = resolve_family(&appearance.code_font, DEFAULT_CODE_FONT, &mut installed);
    set_scale(Scale::of(&appearance, ui, code));
    cx.set_global(appearance);
    apply_kit_theme(cx);
    cx.refresh_windows();
}

/// The Kit theme with the fonts and sizes in use.
pub fn apply_kit_theme(cx: &mut App) {
    let config = super::theme_config(
        &ui_family(),
        &code_family(),
        f32::from(kit_font_size()),
        f32::from(kit_mono_size()),
    );
    match serde_json::from_value::<ThemeConfig>(config) {
        Ok(config) => Theme::update(cx, |t| t.apply_config(&Rc::new(config))),
        Err(e) => eprintln!("crew: theme does not parse: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{FontWeight, SharedString, px};

    use super::{
        Scale, code_family, line, mono, mono_line, relative_line, resolve_family, row, set_scale,
        text, ui_family, ui_px, weight,
    };
    use crate::prefs::{Appearance, Density, LineHeight};

    fn near(a: gpui_kit::Pixels, b: f32) -> bool {
        (f32::from(a) - b).abs() < 0.001
    }

    fn set(a: &Appearance) {
        set_scale(Scale::of(a, None, None));
    }

    #[test]
    fn defaults_change_nothing() {
        set(&Appearance::default());
        for n in [0.5, 1., 11., 12., 13., 28., 48., 264.] {
            assert_eq!(ui_px(n), px(n));
            assert_eq!(text(n), px(n));
            assert_eq!(mono(n), px(n));
            assert_eq!(line(n), px(n));
            assert_eq!(mono_line(n), px(n));
        }
        for n in [1., 4., 28., 44.] {
            assert_eq!(row(n), px(n));
        }
        assert_eq!(weight(FontWeight::MEDIUM), FontWeight::MEDIUM);
        assert_eq!(relative_line(), gpui_kit::phi());
    }

    #[test]
    fn text_follows_its_size() {
        set(&Appearance {
            ui_size: 16,
            code_size: 15,
            ..Appearance::default()
        });
        assert_eq!(text(13.), px(16.));
        assert!(near(text(11.), 11. * 16. / 13.));
        assert_eq!(mono(12.), px(15.));
        assert_eq!(ui_px(264.), px(264.));
        assert!(near(line(20.), 20. * 16. / 13.));
        assert!(near(mono_line(20.), 25.));
        assert_eq!(row(28.), px(34.));
    }

    #[test]
    fn smaller_text_keeps_rows() {
        set(&Appearance {
            ui_size: 12,
            ..Appearance::default()
        });
        assert_eq!(text(13.), px(12.));
        assert_eq!(row(28.), px(28.));
    }

    #[test]
    fn zoom_scales_everything() {
        set(&Appearance {
            zoom: 150,
            ..Appearance::default()
        });
        assert_eq!(ui_px(264.), px(396.));
        assert_eq!(text(12.), px(18.));
        assert_eq!(mono(12.), px(18.));
        assert_eq!(row(28.), px(42.));
        assert!(near(line(20.), 30.));
    }

    #[test]
    fn line_height_and_density_apply() {
        set(&Appearance {
            line_height: LineHeight::Roomy,
            density: Density::Compact,
            ..Appearance::default()
        });
        assert!(near(line(15.), 17.));
        assert_eq!(row(20.), px(17.));
        assert_eq!(text(13.), px(13.));
        set(&Appearance {
            line_height: LineHeight::Compact,
            ..Appearance::default()
        });
        assert!(near(line(30.), 27.));
    }

    #[test]
    fn bolder_steps_one_weight_up() {
        set(&Appearance {
            bolder: true,
            ..Appearance::default()
        });
        assert_eq!(weight(FontWeight::NORMAL), FontWeight::MEDIUM);
        assert_eq!(weight(FontWeight::MEDIUM), FontWeight::SEMIBOLD);
        assert_eq!(weight(FontWeight::SEMIBOLD), FontWeight::BOLD);
        assert_eq!(weight(FontWeight::BLACK), FontWeight::BLACK);
    }

    #[test]
    fn missing_fonts_fall_back_to_the_default() {
        assert_eq!(resolve_family("Geist", "Geist", |_| true), None);
        assert_eq!(resolve_family("Gone Sans", "Geist", |_| false), None);
        assert_eq!(
            resolve_family("Menlo", "Geist Mono", |_| true),
            Some(SharedString::from("Menlo"))
        );
        set_scale(Scale::of(
            &Appearance::default(),
            Some("Helvetica".into()),
            Some("Menlo".into()),
        ));
        assert_eq!(ui_family(), "Helvetica");
        assert_eq!(code_family(), "Menlo");
    }
}
