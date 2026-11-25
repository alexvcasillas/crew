//! Finds the skills Claude Code and Codex can run in a project.
//!
//! Claude Code: a folder scan of Claude's home and the project, plus the
//! installed and enabled plugins. Codex: `skills/list` on a short-lived
//! `codex app-server`, like `model/list`. Only reads skill folders and the
//! plugin manifest and enabled list. Never reads credentials, and never
//! writes into a CLI's home.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crew_core::agent::Runtime;
use crew_core::codex::CodexConn;
use crew_core::codex::proto::{InitializeParams, method};
use crew_proto::{SkillInfo, SkillSource};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use serde::Deserialize;
use tokio::process::Command;

use crate::runner::signal_group;
use crate::service::log;

/// How long Codex may take to list its skills.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A skill as found, before the "Show in /" setting is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub runtime: Runtime,
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub path: String,
}

impl Found {
    /// The skill as the client sees it.
    pub fn info(&self, shown: bool) -> SkillInfo {
        SkillInfo {
            runtime: self.runtime.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            source: self.source.clone(),
            path: self.path.clone(),
            shown,
        }
    }
}

/// What a skill file's frontmatter says.
#[derive(Debug, Default, PartialEq)]
struct Front {
    name: Option<String>,
    description: String,
}

/// Reads the `name` and `description` of a `---` frontmatter block. A file
/// without a closed block, or with nothing readable in it, gives nothing.
fn frontmatter(text: &str) -> Front {
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Front::default();
    }
    let mut block: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        block.push(line);
    }
    if !closed {
        return Front::default();
    }
    let mut front = Front::default();
    let mut i = 0;
    while i < block.len() {
        let line = block[i];
        i += 1;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let (key, value) = (key.trim(), value.trim());
        // A folded or literal block takes the indented lines after it.
        let text = if matches!(value.chars().next(), Some('>' | '|')) {
            let mut parts: Vec<&str> = Vec::new();
            while i < block.len()
                && (block[i].starts_with(char::is_whitespace) || block[i].trim().is_empty())
            {
                parts.push(block[i].trim());
                i += 1;
            }
            parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            unquote(value).to_string()
        };
        match key {
            "name" => front.name = Some(text).filter(|n| !n.is_empty()),
            "description" => front.description = text,
            _ => {}
        }
    }
    front
}

fn unquote(value: &str) -> &str {
    let quoted = |q: char| value.len() >= 2 && value.starts_with(q) && value.ends_with(q);
    if quoted('"') || quoted('\'') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// A name a CLI can be asked for: no spaces or slashes.
fn usable(name: &str) -> bool {
    !name.is_empty() && !name.contains(char::is_whitespace) && !name.contains('/')
}

fn skill_in(dir: &Path, prefix: &str, source: &SkillSource) -> Option<Found> {
    let file = dir.join("SKILL.md");
    let folder = dir.file_name()?.to_string_lossy().into_owned();
    let text = std::fs::read(&file)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())?;
    let front = frontmatter(&text);
    let name = front.name.filter(|n| usable(n)).unwrap_or(folder);
    Some(Found {
        runtime: Runtime::ClaudeCode,
        name: format!("{prefix}{name}"),
        description: front.description,
        source: source.clone(),
        path: file.to_string_lossy().into_owned(),
    })
}

fn command_in(file: &Path, prefix: &str, source: &SkillSource) -> Option<Found> {
    if file.extension()? != "md" {
        return None;
    }
    let stem = file.file_stem()?.to_string_lossy().into_owned();
    let text = std::fs::read(file)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    Some(Found {
        runtime: Runtime::ClaudeCode,
        name: format!("{prefix}{stem}"),
        description: frontmatter(&text).description,
        source: source.clone(),
        path: file.to_string_lossy().into_owned(),
    })
}

/// The skills (`<base>/skills/*/SKILL.md`) and commands
/// (`<base>/commands/*.md`) of one Claude folder, by name.
fn scan_base(base: &Path, prefix: &str, source: &SkillSource) -> Vec<Found> {
    let entries = |dir: PathBuf| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        paths.sort();
        paths
    };
    let mut found: Vec<Found> = entries(base.join("skills"))
        .iter()
        .filter(|p| p.is_dir())
        .filter_map(|p| skill_in(p, prefix, source))
        .collect();
    for file in entries(base.join("commands")) {
        if file.is_file()
            && let Some(command) = command_in(&file, prefix, source)
            && !found.iter().any(|f| f.name == command.name)
        {
            found.push(command);
        }
    }
    found
}

#[derive(Deserialize, Default)]
struct Installed {
    #[serde(default)]
    plugins: std::collections::BTreeMap<String, Vec<InstalledPlugin>>,
}

#[derive(Deserialize)]
struct InstalledPlugin {
    #[serde(default, rename = "installPath")]
    install_path: String,
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Plugin ids (`name@marketplace`) that settings files switch on or off,
/// later files over earlier ones. Only `enabledPlugins` is read.
fn enabled_plugins(files: &[PathBuf]) -> HashSet<String> {
    #[derive(Deserialize, Default)]
    struct Settings {
        #[serde(default, rename = "enabledPlugins")]
        enabled: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let mut on = HashSet::new();
    for file in files {
        let settings: Settings = read_json(file);
        for (id, value) in settings.enabled {
            if value == serde_json::Value::Bool(true) {
                on.insert(id);
            } else {
                on.remove(&id);
            }
        }
    }
    on
}

/// The skills of the plugins Claude Code lists as installed and that
/// settings enable. A plugin that isn't there or can't be read adds none.
fn scan_plugins(claude_home: &Path, root: &Path) -> Vec<Found> {
    let installed: Installed = read_json(&claude_home.join("plugins/installed_plugins.json"));
    let on = enabled_plugins(&[
        claude_home.join("settings.json"),
        root.join(".claude/settings.json"),
        root.join(".claude/settings.local.json"),
    ]);
    let mut found = Vec::new();
    for (id, installs) in &installed.plugins {
        if !on.contains(id) {
            continue;
        }
        let name = id.split('@').next().unwrap_or(id);
        let Some(dir) = installs
            .iter()
            .map(|i| PathBuf::from(&i.install_path))
            .find(|p| !p.as_os_str().is_empty() && p.is_dir())
        else {
            continue;
        };
        found.extend(scan_base(
            &dir,
            &format!("{name}:"),
            &SkillSource::Plugin(name.to_string()),
        ));
    }
    found
}

/// Claude Code's skills for a project. `claude_home` is `~/.claude`. A
/// name in both the project and the user's folder is listed once, as the
/// project's.
pub fn scan_claude(claude_home: Option<&Path>, root: &Path) -> Vec<Found> {
    let mut found = scan_base(&root.join(".claude"), "", &SkillSource::Project);
    if let Some(home) = claude_home {
        for skill in scan_base(home, "", &SkillSource::Global) {
            if !found.iter().any(|f| f.name == skill.name) {
                found.push(skill);
            }
        }
        found.extend(scan_plugins(home, root));
    }
    found
}

#[derive(Deserialize, Default)]
struct SkillsList {
    #[serde(default)]
    data: Vec<SkillsEntry>,
}

#[derive(Deserialize, Default)]
struct SkillsEntry {
    #[serde(default)]
    skills: Vec<CodexSkill>,
}

#[derive(Deserialize)]
struct CodexSkill {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    path: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    scope: String,
    #[serde(default, rename = "pluginId")]
    plugin_id: Option<String>,
}

fn yes() -> bool {
    true
}

/// Codex's skills from a `skills/list` answer. Disabled ones and ones
/// without a name or path can't run, so they are left out.
fn codex_found(answer: serde_json::Value) -> Vec<Found> {
    let list: SkillsList = serde_json::from_value(answer).unwrap_or_default();
    let mut found: Vec<Found> = Vec::new();
    for skill in list.data.into_iter().flat_map(|e| e.skills) {
        if !skill.enabled || skill.name.is_empty() || skill.path.is_empty() {
            continue;
        }
        let source = match (&skill.plugin_id, skill.scope.as_str()) {
            (Some(id), _) if !id.is_empty() => {
                SkillSource::Plugin(id.split('@').next().unwrap_or(id).to_string())
            }
            (_, "repo") => SkillSource::Project,
            _ => SkillSource::Global,
        };
        let skill = Found {
            runtime: Runtime::Codex,
            name: skill.name,
            description: skill.description,
            source,
            path: skill.path,
        };
        // The project's skill wins over a global one of the same name.
        match found.iter().position(|f| f.name == skill.name) {
            Some(at) if skill.source == SkillSource::Project => found[at] = skill,
            Some(_) => {}
            None => found.push(skill),
        }
    }
    found
}

/// What a short-lived `codex app-server` lists for `root`. Empty if it
/// fails or takes too long. Its process group is killed after.
pub async fn scan_codex(bin: &Path, root: &Path, force_reload: bool) -> Vec<Found> {
    let mut cmd = Command::new(bin);
    cmd.arg("app-server").stderr(Stdio::null()).process_group(0);
    let (conn, _incoming) = match CodexConn::spawn(cmd) {
        Ok(spawned) => spawned,
        Err(e) => {
            log(format!("couldn't start `codex app-server`: {e}"));
            return vec![];
        }
    };
    let pgid = conn.pid().map(|p| Pid::from_raw(p as i32));
    let params = serde_json::json!({
        "cwds": [root.to_string_lossy()],
        "forceReload": force_reload,
    });
    let list = async {
        conn.initialize(&InitializeParams::crew(env!("CARGO_PKG_VERSION")))
            .await?;
        conn.call::<_, serde_json::Value>(method::SKILLS_LIST, &params)
            .await
    };
    let listed = tokio::time::timeout(TIMEOUT, list).await;
    signal_group(pgid, Signal::SIGKILL);
    match listed {
        Ok(Ok(answer)) => codex_found(answer),
        Ok(Err(e)) => {
            log(format!("couldn't list the Codex skills: {e}"));
            vec![]
        }
        Err(_) => {
            log(format!(
                "Codex didn't list its skills within {} s",
                TIMEOUT.as_secs()
            ));
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crew_core::agent::Runtime;
    use crew_proto::SkillSource;
    use serde_json::json;

    use crate::skills::{Front, codex_found, frontmatter, scan_claude};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn skill(dir: &Path, folder: &str, text: &str) {
        write(&dir.join("skills").join(folder).join("SKILL.md"), text);
    }

    #[test]
    fn frontmatter_gives_name_and_description() {
        let front = frontmatter("---\nname: \"deploy\"\ndescription: Ships it\n---\nbody");
        assert_eq!(
            front,
            Front {
                name: Some("deploy".into()),
                description: "Ships it".into()
            }
        );
        let folded = frontmatter("---\nname: x\ndescription: >\n  one\n  two\n---\n");
        assert_eq!(folded.description, "one two");
    }

    #[test]
    fn broken_frontmatter_gives_nothing() {
        assert_eq!(frontmatter("no block"), Front::default());
        assert_eq!(
            frontmatter("---\nname: x\ndescription: never closed\n"),
            Front::default()
        );
        assert_eq!(frontmatter(""), Front::default());
    }

    #[test]
    fn claude_skills_come_from_every_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("repo");
        skill(
            &home,
            "unslop",
            "---\nname: unslop\ndescription: Cuts slop\n---\n",
        );
        write(
            &home.join("commands/ship.md"),
            "---\ndescription: Ship it\n---\n",
        );
        skill(
            &root.join(".claude"),
            "lint",
            "---\nname: lint\ndescription: Lints\n---\n",
        );
        write(&root.join(".claude/commands/fmt.md"), "just a body");
        let found = scan_claude(Some(&home), &root);
        let got: Vec<(&str, &str, SkillSource)> = found
            .iter()
            .map(|f| (f.name.as_str(), f.description.as_str(), f.source.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("lint", "Lints", SkillSource::Project),
                ("fmt", "", SkillSource::Project),
                ("unslop", "Cuts slop", SkillSource::Global),
                ("ship", "Ship it", SkillSource::Global),
            ]
        );
        assert!(found.iter().all(|f| f.runtime == Runtime::ClaudeCode));
        assert!(found[0].path.ends_with("lint/SKILL.md"));
    }

    #[test]
    fn project_wins_over_global_of_the_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("repo");
        skill(
            &home,
            "review",
            "---\nname: review\ndescription: global\n---\n",
        );
        skill(
            &root.join(".claude"),
            "review",
            "---\nname: review\ndescription: project\n---\n",
        );
        let found = scan_claude(Some(&home), &root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, SkillSource::Project);
        assert_eq!(found[0].description, "project");
    }

    #[test]
    fn a_broken_skill_file_is_listed_by_folder_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        skill(&home, "messy", "---\nname: messy\n");
        skill(&home, "binary", "\u{0}\u{1}---\n::\n");
        skill(
            &home,
            "spaced",
            "---\nname: two words\ndescription: d\n---\n",
        );
        std::fs::create_dir_all(home.join("skills/empty-folder")).unwrap();
        let found = scan_claude(Some(&home), &tmp.path().join("repo"));
        let got: Vec<(&str, &str)> = found
            .iter()
            .map(|f| (f.name.as_str(), f.description.as_str()))
            .collect();
        assert_eq!(got, [("binary", ""), ("messy", ""), ("spaced", "d")]);
    }

    #[test]
    fn only_enabled_installed_plugins_add_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("repo");
        let on = tmp.path().join("plugins/on");
        let off = tmp.path().join("plugins/off");
        skill(
            &on,
            "brainstorm",
            "---\nname: brainstorm\ndescription: Ideas\n---\n",
        );
        write(&on.join("commands/go.md"), "---\ndescription: Go\n---\n");
        skill(&off, "hidden", "---\nname: hidden\n---\n");
        write(
            &home.join("plugins/installed_plugins.json"),
            &json!({"version": 2, "plugins": {
                "tools@market": [{"scope": "user", "installPath": on}],
                "other@market": [{"scope": "user", "installPath": off}],
                "gone@market": [{"scope": "user", "installPath": "/work/missing"}],
            }})
            .to_string(),
        );
        write(
            &home.join("settings.json"),
            &json!({"enabledPlugins": {
                "tools@market": true, "other@market": false, "gone@market": true
            }, "apiKeyHelper": "never read"})
            .to_string(),
        );
        let found = scan_claude(Some(&home), &root);
        let got: Vec<(&str, SkillSource)> = found
            .iter()
            .map(|f| (f.name.as_str(), f.source.clone()))
            .collect();
        let plugin = SkillSource::Plugin("tools".into());
        assert_eq!(
            got,
            [
                ("tools:brainstorm", plugin.clone()),
                ("tools:go", plugin.clone())
            ]
        );
        // A project's settings can enable one too, or switch it off again.
        write(
            &root.join(".claude/settings.local.json"),
            &json!({"enabledPlugins": {"tools@market": false}}).to_string(),
        );
        assert!(scan_claude(Some(&home), &root).is_empty());
    }

    #[test]
    fn a_missing_home_lists_the_project_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        skill(&root.join(".claude"), "lint", "---\nname: lint\n---\n");
        assert_eq!(scan_claude(None, &root).len(), 1);
        assert!(scan_claude(Some(&tmp.path().join("nope")), &tmp.path().join("x")).is_empty());
    }

    #[test]
    fn codex_skills_map_scope_and_drop_the_unrunnable() {
        let found = codex_found(
            json!({"data": [{"cwd": "/work/demo", "errors": [], "skills": [
                {"name": "review", "description": "global", "path": "/home/s/review/SKILL.md",
                 "enabled": true, "scope": "user"},
                {"name": "review", "description": "project", "path": "/work/demo/.codex/skills/review/SKILL.md",
                 "enabled": true, "scope": "repo"},
                {"name": "plug", "description": "p", "path": "/p/SKILL.md", "enabled": true,
                 "scope": "user", "pluginId": "tools@market"},
                {"name": "off", "description": "", "path": "/o/SKILL.md", "enabled": false, "scope": "user"},
                {"name": "", "description": "", "path": "/n/SKILL.md", "enabled": true, "scope": "user"}
            ]}]}),
        );
        let got: Vec<(&str, &str, SkillSource)> = found
            .iter()
            .map(|f| (f.name.as_str(), f.description.as_str(), f.source.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("review", "project", SkillSource::Project),
                ("plug", "p", SkillSource::Plugin("tools".into())),
            ]
        );
        assert!(found.iter().all(|f| f.runtime == Runtime::Codex));
        assert!(codex_found(json!("junk")).is_empty());
    }
}
