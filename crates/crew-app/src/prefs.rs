//! The app prefs file in the Crew home: choices only the app cares about,
//! one JSON object with a key per choice.

use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;

/// The file in the Crew home that remembers app-only choices.
pub const PREFS_FILE: &str = "app-prefs.json";

/// One write at a time, so two keys saved at once both stay.
static WRITING: Mutex<()> = Mutex::new(());

/// The value saved under `key`, if the file has one.
pub fn read(crew_home: &Path, key: &str) -> Option<Value> {
    let text = std::fs::read_to_string(crew_home.join(PREFS_FILE)).ok()?;
    let mut value: Value = serde_json::from_str(&text).ok()?;
    value.get_mut(key).map(Value::take)
}

/// Saves `value` under `key`, keeping the other keys in the file.
pub fn write(crew_home: &Path, key: &str, value: Value) -> std::io::Result<()> {
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let path = crew_home.join(PREFS_FILE);
    let mut all = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    all[key] = value;
    std::fs::create_dir_all(crew_home)?;
    let temp = crew_home.join(format!("{PREFS_FILE}.tmp"));
    std::fs::write(&temp, all.to_string())?;
    std::fs::rename(&temp, path)
}

/// The prefs key the appearance settings live under.
pub const APPEARANCE_KEY: &str = "appearance";

/// Interface text sizes on offer, in px.
pub const UI_SIZES: [u8; 6] = [12, 13, 14, 15, 16, 18];
/// Code text sizes on offer, in px.
pub const CODE_SIZES: [u8; 6] = [11, 12, 13, 14, 15, 16];
pub const ZOOM_MIN: u16 = 50;
pub const ZOOM_MAX: u16 = 200;
pub const ZOOM_STEP: u16 = 10;

/// How tall lines of text are, for their size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LineHeight {
    Compact,
    #[default]
    Comfortable,
    Roomy,
}

impl LineHeight {
    /// Lines are this many times the text size.
    pub fn multiplier(self) -> f32 {
        match self {
            Self::Compact => 1.35,
            Self::Comfortable => 1.5,
            Self::Roomy => 1.7,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
            Self::Roomy => "roomy",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [Self::Compact, Self::Comfortable, Self::Roomy]
            .into_iter()
            .find(|l| l.name() == name)
    }
}

/// How tall rows are and how much padding they get.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Density {
    Compact,
    #[default]
    Comfortable,
}

impl Density {
    /// Row heights and vertical paddings are this many times their size.
    pub fn factor(self) -> f32 {
        match self {
            Self::Compact => 0.85,
            Self::Comfortable => 1.0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [Self::Compact, Self::Comfortable]
            .into_iter()
            .find(|d| d.name() == name)
    }
}

/// What the sidebar lists first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SidebarMode {
    #[default]
    Projects,
    Threads,
}

impl SidebarMode {
    fn name(self) -> &'static str {
        match self {
            Self::Projects => "projects",
            Self::Threads => "threads",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [Self::Projects, Self::Threads]
            .into_iter()
            .find(|m| m.name() == name)
    }
}

/// How the app looks: text sizes, fonts, line height, weight, density,
/// zoom and the sidebar's mode. The default is the app's original look.
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    /// Interface text size in px.
    pub ui_size: u8,
    /// Code text size in px.
    pub code_size: u8,
    /// Interface font family.
    pub ui_font: String,
    /// Code font family.
    pub code_font: String,
    pub line_height: LineHeight,
    /// Draws every weight one step heavier.
    pub bolder: bool,
    pub density: Density,
    /// The whole window's scale, in percent.
    pub zoom: u16,
    pub sidebar_mode: SidebarMode,
    /// Keys a newer version wrote, kept as they are on save.
    pub extra: serde_json::Map<String, Value>,
}

pub const DEFAULT_UI_FONT: &str = "Geist";
pub const DEFAULT_CODE_FONT: &str = "Geist Mono";

impl Default for Appearance {
    fn default() -> Self {
        Self {
            ui_size: 13,
            code_size: 12,
            ui_font: DEFAULT_UI_FONT.into(),
            code_font: DEFAULT_CODE_FONT.into(),
            line_height: LineHeight::default(),
            bolder: false,
            density: Density::default(),
            zoom: 100,
            sidebar_mode: SidebarMode::default(),
            extra: serde_json::Map::new(),
        }
    }
}

const KNOWN_KEYS: [&str; 9] = [
    "ui_size",
    "code_size",
    "ui_font",
    "code_font",
    "line_height",
    "bolder",
    "density",
    "zoom",
    "sidebar_mode",
];

impl Appearance {
    /// What the prefs file in `crew_home` holds. Anything missing or
    /// unreadable takes its default.
    pub fn load(crew_home: &Path) -> Self {
        read(crew_home, APPEARANCE_KEY)
            .map(|v| Self::from_value(&v))
            .unwrap_or_default()
    }

    /// Writes it to the prefs file. Blocks, so call it off the UI thread.
    pub fn save(&self, crew_home: &Path) -> std::io::Result<()> {
        write(crew_home, APPEARANCE_KEY, self.to_value())
    }

    /// Reads each field on its own, so one bad field keeps the others.
    pub fn from_value(value: &Value) -> Self {
        let mut a = Self::default();
        let Some(map) = value.as_object() else {
            return a;
        };
        let int = |key: &str| map.get(key).and_then(Value::as_u64);
        let text = |key: &str| {
            map.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        };
        if let Some(n) = int("ui_size") {
            a.ui_size = nearest(n, &UI_SIZES);
        }
        if let Some(n) = int("code_size") {
            a.code_size = nearest(n, &CODE_SIZES);
        }
        if let Some(f) = text("ui_font") {
            a.ui_font = f.to_string();
        }
        if let Some(f) = text("code_font") {
            a.code_font = f.to_string();
        }
        if let Some(l) = text("line_height").and_then(LineHeight::parse) {
            a.line_height = l;
        }
        if let Some(b) = map.get("bolder").and_then(Value::as_bool) {
            a.bolder = b;
        }
        if let Some(d) = text("density").and_then(Density::parse) {
            a.density = d;
        }
        if let Some(z) = int("zoom") {
            let step = u64::from(ZOOM_STEP);
            let on_grid = (z + step / 2) / step * step;
            a.zoom = on_grid.clamp(u64::from(ZOOM_MIN), u64::from(ZOOM_MAX)) as u16;
        }
        if let Some(m) = text("sidebar_mode").and_then(SidebarMode::parse) {
            a.sidebar_mode = m;
        }
        a.extra = map
            .iter()
            .filter(|(k, _)| !KNOWN_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        a
    }

    pub fn to_value(&self) -> Value {
        let mut map = self.extra.clone();
        map.insert("ui_size".into(), self.ui_size.into());
        map.insert("code_size".into(), self.code_size.into());
        map.insert("ui_font".into(), self.ui_font.clone().into());
        map.insert("code_font".into(), self.code_font.clone().into());
        map.insert("line_height".into(), self.line_height.name().into());
        map.insert("bolder".into(), self.bolder.into());
        map.insert("density".into(), self.density.name().into());
        map.insert("zoom".into(), self.zoom.into());
        map.insert("sidebar_mode".into(), self.sidebar_mode.name().into());
        Value::Object(map)
    }
}

/// The offered size closest to `n`, the smaller on a tie.
fn nearest(n: u64, sizes: &[u8]) -> u8 {
    sizes
        .iter()
        .copied()
        .min_by_key(|&s| n.abs_diff(u64::from(s)))
        .unwrap_or(sizes[0])
}

/// `zoom` one step bigger (`direction` 1) or smaller (-1), on the 10%
/// grid and within bounds.
pub fn step_zoom(zoom: u16, direction: i8) -> u16 {
    let grid = zoom / ZOOM_STEP * ZOOM_STEP;
    let next = if direction > 0 {
        grid + ZOOM_STEP
    } else if grid == zoom {
        grid.saturating_sub(ZOOM_STEP)
    } else {
        grid
    };
    next.clamp(ZOOM_MIN, ZOOM_MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        APPEARANCE_KEY, Appearance, CODE_SIZES, Density, LineHeight, PREFS_FILE, SidebarMode,
        ZOOM_MIN, read, step_zoom, write,
    };

    #[test]
    fn keys_are_saved_side_by_side() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(read(home.path(), "editor"), None);
        write(home.path(), "editor", "Zed".into()).unwrap();
        write(home.path(), "recent", serde_json::json!({"threads": []})).unwrap();
        assert_eq!(read(home.path(), "editor"), Some("Zed".into()));
        assert_eq!(
            read(home.path(), "recent"),
            Some(serde_json::json!({"threads": []}))
        );
        std::fs::write(home.path().join(PREFS_FILE), "not json").unwrap();
        assert_eq!(read(home.path(), "editor"), None);
        write(home.path(), "editor", "Cursor".into()).unwrap();
        assert_eq!(read(home.path(), "editor"), Some("Cursor".into()));
    }

    #[test]
    fn appearance_defaults_to_todays_look() {
        let a = Appearance::default();
        assert_eq!((a.ui_size, a.code_size), (13, 12));
        assert_eq!(
            (a.ui_font.as_str(), a.code_font.as_str()),
            ("Geist", "Geist Mono")
        );
        assert_eq!(a.line_height, LineHeight::Comfortable);
        assert!(!a.bolder);
        assert_eq!(a.density, Density::Comfortable);
        assert_eq!(a.zoom, 100);
        assert_eq!(a.sidebar_mode, SidebarMode::Projects);
    }

    #[test]
    fn appearance_round_trips_and_keeps_unknown_fields() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(Appearance::load(home.path()), Appearance::default());
        write(
            home.path(),
            APPEARANCE_KEY,
            serde_json::json!({"ui_size": 16, "glow": true}),
        )
        .unwrap();
        let mut a = Appearance::load(home.path());
        assert_eq!(a.ui_size, 16);
        a.code_size = 15;
        a.bolder = true;
        a.line_height = LineHeight::Roomy;
        a.density = Density::Compact;
        a.sidebar_mode = SidebarMode::Threads;
        a.zoom = 130;
        a.save(home.path()).unwrap();
        let saved = read(home.path(), APPEARANCE_KEY).unwrap();
        assert_eq!(saved["glow"], serde_json::json!(true));
        assert_eq!(saved["line_height"], serde_json::json!("roomy"));
        assert_eq!(Appearance::load(home.path()), a);
    }

    #[test]
    fn appearance_tolerates_garbage() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(PREFS_FILE), "{{ not json").unwrap();
        assert_eq!(Appearance::load(home.path()), Appearance::default());
        write(home.path(), APPEARANCE_KEY, "big".into()).unwrap();
        assert_eq!(Appearance::load(home.path()), Appearance::default());
        write(
            home.path(),
            APPEARANCE_KEY,
            serde_json::json!({
                "ui_size": "huge",
                "code_size": 99,
                "ui_font": "",
                "line_height": "airy",
                "bolder": 1,
                "zoom": 7,
                "density": "compact",
            }),
        )
        .unwrap();
        let a = Appearance::load(home.path());
        assert_eq!(a.ui_size, 13);
        assert_eq!(a.code_size, CODE_SIZES[CODE_SIZES.len() - 1]);
        assert_eq!(a.ui_font, "Geist");
        assert_eq!(a.line_height, LineHeight::Comfortable);
        assert!(!a.bolder);
        assert_eq!(a.zoom, ZOOM_MIN);
        assert_eq!(a.density, Density::Compact);

        // Only the offered sizes, and zoom on its 10% grid.
        write(
            home.path(),
            APPEARANCE_KEY,
            serde_json::json!({"ui_size": 17, "code_size": 3, "zoom": 105}),
        )
        .unwrap();
        let a = Appearance::load(home.path());
        assert_eq!((a.ui_size, a.code_size, a.zoom), (16, 11, 110));
    }

    #[test]
    fn zoom_steps_by_ten_within_bounds() {
        assert_eq!(step_zoom(100, 1), 110);
        assert_eq!(step_zoom(100, -1), 90);
        assert_eq!(step_zoom(200, 1), 200);
        assert_eq!(step_zoom(50, -1), 50);
        assert_eq!(step_zoom(105, 1), 110);
    }
}
