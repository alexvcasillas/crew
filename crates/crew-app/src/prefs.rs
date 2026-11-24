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

#[cfg(test)]
mod tests {
    use super::{PREFS_FILE, read, write};

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
}
