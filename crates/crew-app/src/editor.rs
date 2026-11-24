//! The editor that "Open in editor" uses: which editors are installed,
//! which one you chose, and the `open` command that launches it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The label of the folder viewer, always on offer.
pub const FINDER: &str = "Finder";

/// Editors Crew looks for, in the order it prefers them.
pub const KNOWN: [&str; 5] = [
    "Visual Studio Code",
    "Cursor",
    "Zed",
    "Xcode",
    "Sublime Text",
];

/// The known editors found in `/Applications` or `<home>/Applications`.
/// `exists` answers whether a path is there.
pub fn detect(home: Option<&Path>, exists: impl Fn(&Path) -> bool) -> Vec<String> {
    KNOWN
        .iter()
        .filter(|name| {
            let bundle = format!("{name}.app");
            exists(&Path::new("/Applications").join(&bundle))
                || home.is_some_and(|h| exists(&h.join("Applications").join(&bundle)))
        })
        .map(|name| name.to_string())
        .collect()
}

/// The installed editors on this Mac.
pub fn detect_installed() -> Vec<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    detect(home.as_deref(), |p| p.exists())
}

/// The app to open: your `chosen` one if it is still on offer, else the
/// first detected editor, else Finder. `None` means Finder.
pub fn resolve(chosen: Option<&str>, detected: &[String]) -> Option<String> {
    match chosen {
        Some(FINDER) => None,
        Some(name) if detected.iter().any(|d| d == name) => Some(name.to_string()),
        _ => detected.first().cloned(),
    }
}

/// The arguments for `open`: the worktree, plus `file` for an editor.
/// Finder opens the folder only.
pub fn open_args(app: Option<&str>, worktree: &Path, file: Option<&Path>) -> Vec<OsString> {
    let mut args = Vec::new();
    match app {
        Some(app) => {
            args.push("-a".into());
            args.push(app.into());
            args.push(worktree.into());
            if let Some(file) = file {
                args.push(file.into());
            }
        }
        None => args.push(worktree.into()),
    }
    args
}

/// Runs `open` with the arguments. Blocks, so call it off the UI thread.
pub type Opener = Arc<dyn Fn(Vec<OsString>) -> Result<(), String> + Send + Sync>;

/// Runs the real `open`.
pub fn system_opener() -> Opener {
    Arc::new(|args| {
        let status = std::process::Command::new("open")
            .args(&args)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("open exited with {status}"))
        }
    })
}

/// What to tell you when the editor didn't open.
pub fn open_failed(app: Option<&str>) -> String {
    match app {
        Some(app) => format!("{app} didn't open. Check that it is installed."),
        None => "Finder couldn't open the folder. Refresh to check it still exists.".into(),
    }
}

/// The editor you chose, if you chose one: an app name or [`FINDER`].
pub fn load_choice(crew_home: &Path) -> Option<String> {
    crate::prefs::read(crew_home, "editor")?
        .as_str()
        .map(str::to_string)
}

/// Remembers your choice, keeping other keys in the file.
pub fn save_choice(crew_home: &Path, choice: &str) -> std::io::Result<()> {
    crate::prefs::write(crew_home, "editor", choice.into())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{FINDER, detect, load_choice, open_args, open_failed, resolve, save_choice};

    fn installed(paths: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |p| set.contains(p)
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detection_checks_both_applications_folders_in_the_known_order() {
        let found = detect(
            Some(Path::new("/Users/me")),
            installed(&[
                "/Applications/Zed.app",
                "/Users/me/Applications/Cursor.app",
                "/Applications/Sublime Text.app",
                "/Applications/Notes.app",
            ]),
        );
        assert_eq!(found, names(&["Cursor", "Zed", "Sublime Text"]));
        assert!(detect(None, installed(&[])).is_empty());
    }

    #[test]
    fn the_default_is_the_first_detected_editor_else_finder() {
        let found = names(&["Cursor", "Zed"]);
        assert_eq!(resolve(None, &found), Some("Cursor".into()));
        assert_eq!(resolve(None, &[]), None);
    }

    #[test]
    fn a_choice_wins_when_it_is_still_on_offer() {
        let found = names(&["Cursor", "Zed"]);
        assert_eq!(resolve(Some("Zed"), &found), Some("Zed".into()));
        assert_eq!(resolve(Some(FINDER), &found), None);
        // An editor you removed falls back to the default.
        assert_eq!(resolve(Some("Xcode"), &found), Some("Cursor".into()));
    }

    #[test]
    fn finder_opens_the_folder_and_an_editor_gets_the_file_too() {
        let wt = Path::new("/work/demo/wt");
        let file = Path::new("/work/demo/wt/src/a.rs");
        let os = |parts: &[&str]| parts.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(open_args(None, wt, Some(file)), os(&["/work/demo/wt"]));
        assert_eq!(
            open_args(Some("Zed"), wt, None),
            os(&["-a", "Zed", "/work/demo/wt"])
        );
        assert_eq!(
            open_args(Some("Visual Studio Code"), wt, Some(file)),
            os(&[
                "-a",
                "Visual Studio Code",
                "/work/demo/wt",
                "/work/demo/wt/src/a.rs"
            ])
        );
    }

    #[test]
    fn failures_read_as_one_sentence() {
        assert_eq!(
            open_failed(Some("Zed")),
            "Zed didn't open. Check that it is installed."
        );
        assert!(open_failed(None).starts_with("Finder"));
    }

    #[test]
    fn the_choice_is_remembered_in_the_home() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(load_choice(home.path()), None);
        save_choice(home.path(), "Zed").unwrap();
        assert_eq!(load_choice(home.path()), Some("Zed".into()));
        save_choice(home.path(), FINDER).unwrap();
        assert_eq!(load_choice(home.path()), Some(FINDER.into()));
        std::fs::write(home.path().join("app-prefs.json"), "not json").unwrap();
        assert_eq!(load_choice(home.path()), None);
    }

    #[test]
    fn saving_leaves_no_temp_file_and_keeps_other_keys() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("app-prefs.json"), r#"{"other":1}"#).unwrap();
        save_choice(home.path(), "Zed").unwrap();
        let names: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["app-prefs.json"]);
        let text = std::fs::read_to_string(home.path().join("app-prefs.json")).unwrap();
        assert!(text.contains("\"other\""));
    }
}
