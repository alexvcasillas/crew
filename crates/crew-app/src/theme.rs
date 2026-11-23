//! Design tokens from `assets/tokens.json` (dark), and the Kit theme built
//! from them.

use std::collections::HashMap;
use std::sync::OnceLock;

use gpui_kit::{Hsla, Rgba};

const TOKENS_JSON: &str = include_str!("../assets/tokens.json");

/// The dark color token `name`, with `{alias}` values resolved.
///
/// # Panics
///
/// Panics if `name` is not a color token in `tokens.json`.
pub fn color(name: &str) -> Hsla {
    match palette().get(name) {
        Some(&c) => c.into(),
        None => panic!("unknown color token `{name}`"),
    }
}

/// Token name to resolved dark color, parsed once.
fn palette() -> &'static HashMap<String, Rgba> {
    static PALETTE: OnceLock<HashMap<String, Rgba>> = OnceLock::new();
    PALETTE.get_or_init(|| {
        let tokens: serde_json::Value =
            serde_json::from_str(TOKENS_JSON).expect("tokens.json is valid JSON");
        let raw: HashMap<&str, &str> = tokens["color"]["tokens"]
            .as_array()
            .expect("color.tokens is an array")
            .iter()
            .map(|t| {
                let name = t["name"].as_str().expect("token name");
                let dark = t["value"]["dark"].as_str().expect("token dark value");
                (name, dark)
            })
            .collect();
        raw.keys()
            .map(|&name| (name.to_owned(), resolve(&raw, name)))
            .collect()
    })
}

fn resolve(raw: &HashMap<&str, &str>, name: &str) -> Rgba {
    let mut value = raw[name];
    // The bound guards against alias cycles.
    for _ in 0..raw.len() {
        match value.strip_prefix('{').and_then(|v| v.strip_suffix('}')) {
            Some(target) => value = raw[target],
            None => return parse_hex(value),
        }
    }
    panic!("alias cycle at color token `{name}`")
}

/// Parses `#rrggbb` or `#rrggbbaa`.
fn parse_hex(value: &str) -> Rgba {
    let hex = value
        .strip_prefix('#')
        .filter(|h| matches!(h.len(), 6 | 8))
        .unwrap_or_else(|| panic!("bad color `{value}`"));
    let channel = |i: usize| {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16)
            .unwrap_or_else(|_| panic!("bad color `{value}`"));
        f32::from(byte) / 255.0
    };
    Rgba {
        r: channel(0),
        g: channel(2),
        b: channel(4),
        a: if hex.len() == 8 { channel(6) } else { 1.0 },
    }
}

/// `#rrggbbaa` for a dark token.
fn hex(name: &str) -> String {
    let c = palette()[name];
    let byte = |v: f32| (v * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(c.r),
        byte(c.g),
        byte(c.b),
        byte(c.a)
    )
}

/// Kit theme color key to token.
pub const KIT_COLORS: &[(&str, &str)] = &[
    ("background", "bg-100"),
    ("foreground", "text-primary"),
    ("border", "border-200"),
    ("input.border", "border-200"),
    ("ring", "accent-blue"),
    ("caret", "text-primary"),
    ("selection.background", "selection"),
    ("muted.background", "surface-300"),
    ("muted.foreground", "text-secondary"),
    ("accent.background", "surface-300"),
    ("accent.foreground", "text-primary"),
    ("popover.background", "surface-raised"),
    ("popover.foreground", "text-primary"),
    ("primary.background", "primary"),
    ("primary.hover.background", "primary-hover"),
    ("primary.active.background", "primary-hover"),
    ("primary.foreground", "on-primary"),
    ("secondary.background", "bg-100"),
    ("secondary.hover.background", "surface-100"),
    ("secondary.active.background", "surface-200"),
    ("secondary.foreground", "text-primary"),
    ("link.foreground", "accent-blue"),
    ("link.hover.foreground", "accent-blue"),
    ("link.active.foreground", "accent-blue"),
    ("list.background", "bg-100"),
    ("list.hover.background", "surface-100"),
    ("list.active.background", "surface-200"),
    ("list.active.border", "surface-200"),
    ("danger.background", "danger"),
    ("danger.foreground", "danger"),
    ("warning.background", "warning"),
    ("warning.foreground", "warning"),
    ("success.background", "success"),
    ("success.foreground", "success"),
    ("info.background", "accent-blue"),
    ("info.foreground", "accent-blue"),
    ("title_bar.background", "bg-100"),
    ("title_bar.border", "border-100"),
    ("status_bar.background", "bg-100"),
    ("status_bar.border", "border-100"),
    ("sidebar.background", "bg-100"),
    ("sidebar.border", "border-100"),
    ("tab_bar.background", "bg-100"),
    ("tab.background", "bg-100"),
    ("tab.active.background", "bg-100"),
    ("tab.active.foreground", "text-primary"),
    ("tab.foreground", "text-secondary"),
    ("overlay", "overlay"),
    ("window.border", "border-100"),
    ("scrollbar.thumb.background", "border-200"),
    ("scrollbar.thumb.hover.background", "text-disabled"),
];

/// The Kit theme file: dark mode, the given font families and the tokens
/// on the Kit's colors.
pub fn theme_config(ui_font: &str, mono_font: &str) -> serde_json::Value {
    let colors: serde_json::Map<String, serde_json::Value> = KIT_COLORS
        .iter()
        .map(|&(key, token)| (key.to_string(), hex(token).into()))
        .collect();
    serde_json::json!({
        "name": "Crew",
        "mode": "dark",
        "font.family": ui_font,
        "font.size": 13.0,
        "mono_font.family": mono_font,
        "mono_font.size": 13.0,
        "radius": 6,
        "radius.lg": 8,
        "shadow": false,
        "colors": colors,
    })
}

#[cfg(test)]
mod tests {
    use super::{KIT_COLORS, TOKENS_JSON, color, hex, palette, theme_config};
    use gpui_kit::component::ThemeConfig;

    #[test]
    fn theme_every_token_resolves() {
        let tokens: serde_json::Value = serde_json::from_str(TOKENS_JSON).unwrap();
        let names: Vec<&str> = tokens["color"]["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.is_empty());
        for name in names {
            color(name);
        }
        for (_, token) in KIT_COLORS {
            assert!(palette().contains_key(*token), "{token}");
        }
    }

    #[test]
    fn hex_keeps_alpha() {
        assert_eq!(hex("bg-100"), "#0a0a0aff");
        assert_eq!(hex("overlay"), "#000000b3");
    }

    #[test]
    fn theme_config_parses_as_dark() {
        let config: ThemeConfig =
            serde_json::from_value(theme_config("Geist", "Geist Mono")).unwrap();
        assert!(config.mode.is_dark());
    }
}
