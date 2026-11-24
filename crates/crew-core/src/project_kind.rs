//! What a project folder holds and how agents work in it.

use std::collections::VecDeque;
use std::path::{Component, Path};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// How deep under a folder git repos are looked for.
pub const MAX_DEPTH: usize = 3;
/// Most folders read while looking for repos, so a huge tree stays fast.
pub const MAX_DIRS: usize = 5_000;
/// Folders never looked into for repos.
const SKIPPED: [&str; 2] = ["node_modules", "target"];

/// Said when a folder without git asks for a copy per thread.
pub const NEEDS_REPO: &str = "Needs at least one git repo in the folder.";

/// What a project folder is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectKind {
    /// The folder is in a git checkout.
    #[default]
    Repo,
    /// The folder holds git repos, at these paths relative to it.
    MultiRepo { repos: Vec<String> },
    /// No git at all.
    Plain,
}

impl ProjectKind {
    /// The repos agents' changes live in, relative to the project folder.
    /// A `Repo` is the folder itself, written as an empty path.
    pub fn repos(&self) -> Vec<String> {
        match self {
            ProjectKind::Repo => vec![String::new()],
            ProjectKind::MultiRepo { repos } => repos.clone(),
            ProjectKind::Plain => Vec::new(),
        }
    }

    /// The work mode a new project of this kind starts with.
    pub fn default_mode(&self) -> WorkMode {
        match self {
            ProjectKind::Plain => WorkMode::InPlace,
            _ => WorkMode::OwnCopy,
        }
    }

    /// Whether agents can work in `mode` here. A copy per thread needs git.
    pub fn allows(&self, mode: WorkMode) -> Result<(), String> {
        match (self, mode) {
            (ProjectKind::Plain, WorkMode::OwnCopy) => Err(NEEDS_REPO.into()),
            _ => Ok(()),
        }
    }
}

/// Where agents write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    /// Each thread gets its own worktree, or a folder of worktrees.
    #[default]
    OwnCopy,
    /// Agents edit the project folder itself, one writer at a time.
    InPlace,
}

/// Works out what `path` is. A folder with `.git` is a `Repo`. Otherwise
/// repos are looked for at most [`MAX_DEPTH`] levels down, never inside
/// another repo (so submodules and nested repos count with their parent),
/// hidden folders, `node_modules` or `target`. If none is found but the
/// folder sits inside a checkout, it is a `Repo` too.
pub fn detect_kind(path: &Path) -> ProjectKind {
    if !path.is_dir() {
        return ProjectKind::Plain;
    }
    if path.join(".git").exists() {
        return ProjectKind::Repo;
    }
    let repos = find_repos(path);
    if !repos.is_empty() {
        return ProjectKind::MultiRepo { repos };
    }
    if inside_checkout(path) {
        ProjectKind::Repo
    } else {
        ProjectKind::Plain
    }
}

/// Repos under `path`, relative to it and sorted, found as
/// [`detect_kind`] says.
pub fn find_repos(path: &Path) -> Vec<String> {
    let mut repos = Vec::new();
    let mut queue: VecDeque<(std::path::PathBuf, usize)> = VecDeque::new();
    queue.push_back((path.to_path_buf(), 0));
    let mut read = 0usize;
    while let Some((dir, depth)) = queue.pop_front() {
        if read >= MAX_DIRS {
            break;
        }
        read += 1;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<_> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                !name.starts_with('.') && !SKIPPED.contains(&name.as_ref())
            })
            .map(|e| e.path())
            .collect();
        children.sort();
        for child in children {
            if child.join(".git").exists() {
                if let Ok(rel) = child.strip_prefix(path) {
                    repos.push(rel.to_string_lossy().into_owned());
                }
            } else if depth + 1 < MAX_DEPTH {
                queue.push_back((child, depth + 1));
            }
        }
    }
    repos.sort();
    repos
}

fn inside_checkout(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.starts_with(b"true"))
}

/// The repo of `repos` that holds `path` (relative to the project folder),
/// and the path inside it. The longest match wins; an empty repo is the
/// folder itself and holds everything.
pub fn repo_of<'a>(repos: &'a [String], path: &'a str) -> Option<(&'a str, &'a str)> {
    repos
        .iter()
        .filter_map(|repo| {
            if repo.is_empty() {
                return Some((repo.as_str(), path));
            }
            let rest = path.strip_prefix(repo.as_str())?.strip_prefix('/')?;
            Some((repo.as_str(), rest))
        })
        .max_by_key(|(repo, _)| repo.len())
}

/// `path` relative to `root` if it lies inside it, with `.` parts dropped.
/// A relative `path` is taken as relative to `root` already. Paths that
/// leave `root` give `None`.
pub fn relative_to(root: &Path, path: &str) -> Option<String> {
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        match p.strip_prefix(root) {
            Ok(rel) => rel.to_path_buf(),
            // The agent may name the folder with its links resolved.
            Err(_) => p
                .strip_prefix(root.canonicalize().ok()?)
                .ok()?
                .to_path_buf(),
        }
    } else {
        p.to_path_buf()
    };
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Whether a setup file looks like it holds secrets, so it starts left
/// out: keys, certificates, and env files for production.
pub fn looks_secret(path: &str) -> bool {
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    let lower = name.to_lowercase();
    ["pem", "key", "p12", "pfx"]
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
        || is_live_keys(path)
}

/// Whether the file's name says it is for production, where the board
/// says "live keys, left out".
pub fn is_live_keys(path: &str) -> bool {
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    let lower = name.to_lowercase();
    lower.contains("production") || lower.split(['.', '-', '_']).any(|part| part == "prod")
}
