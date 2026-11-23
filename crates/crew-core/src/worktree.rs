//! Git worktrees for threads. Everything shells out to the `git` binary.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Errors from worktree operations.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("git {} failed: {stderr}", args.join(" "))]
    Git { args: Vec<String>, stderr: String },
    #[error("could not run git: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0:?} must be a relative path inside the project")]
    BadPath(String),
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    /// Git's status letter: `A`, `M`, `D` or `T`.
    pub status: char,
    /// Lines added. Binary files count 0.
    pub added: u32,
    /// Lines removed. Binary files count 0.
    pub removed: u32,
}

/// Changes in a worktree compared to a commit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub files: Vec<FileChange>,
    pub patch: String,
}

/// Creates `dest` as a worktree on a new `branch` from `main_branch`.
/// Uncommitted changes in `root` are not carried over. Returns the base sha.
pub fn create(root: &Path, main_branch: &str, dest: &Path, branch: &str) -> Result<String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    git(
        root,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-q"),
            OsStr::new("-b"),
            OsStr::new(branch),
            dest.as_os_str(),
            OsStr::new(main_branch),
        ],
    )?;
    head(dest)
}

/// Copies files (paths relative to the project) from `root` to `dest`,
/// creating folders as needed. Returns the paths that did not exist in `root`.
pub fn copy_files(root: &Path, dest: &Path, files: &[String]) -> Result<Vec<String>> {
    if let Some(bad) = files.iter().find(|f| !is_plain_relative(f)) {
        return Err(WorktreeError::BadPath(bad.clone()));
    }
    let mut skipped = Vec::new();
    for file in files {
        let from = root.join(file);
        if !from.is_file() {
            skipped.push(file.clone());
            continue;
        }
        let to = dest.join(file);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&from, &to)?;
    }
    Ok(skipped)
}

fn is_plain_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Everything in `dest` that differs from `base`: commits, staged and
/// unstaged edits, and untracked files that are not ignored. The real
/// index is left untouched.
pub fn diff(dest: &Path, base: &str) -> Result<Diff> {
    let index = TempIndex::new(dest)?;
    index.git(["add", "-A"])?;
    let names = index.git([
        "diff",
        "--cached",
        "--no-renames",
        "-z",
        "--name-status",
        base,
    ])?;
    let counts = index.git(["diff", "--cached", "--no-renames", "-z", "--numstat", base])?;
    let patch = index.git(["diff", "--cached", "--no-renames", base])?;

    let mut files = Vec::new();
    let mut fields = names.split('\0').filter(|s| !s.is_empty());
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        files.push(FileChange {
            path: path.to_string(),
            status: status.chars().next().unwrap_or('M'),
            added: 0,
            removed: 0,
        });
    }
    for entry in counts.split('\0').filter(|s| !s.is_empty()) {
        let mut parts = entry.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let Some(f) = files.iter_mut().find(|f| f.path == path) {
            f.added = added.parse().unwrap_or(0);
            f.removed = removed.parse().unwrap_or(0);
        }
    }
    Ok(Diff { files, patch })
}

/// Stages everything in `dest` and commits it. Uses the repo's git identity,
/// or `Crew <crew@localhost>` for any part that is not set. Returns the sha.
pub fn commit(dest: &Path, message: &str) -> Result<String> {
    git(dest, ["add", "-A"])?;
    let mut args: Vec<String> = Vec::new();
    for (key, fallback) in [("user.name", "Crew"), ("user.email", "crew@localhost")] {
        if config(dest, key)?.is_none() {
            args.extend(["-c".into(), format!("{key}={fallback}")]);
        }
    }
    args.extend(["commit", "-q", "-m", message].map(String::from));
    git(dest, &args)?;
    head(dest)
}

/// Removes the worktree at `dest`, discarding its changes, and optionally
/// deletes `branch`. A `dest` that is already gone is pruned instead.
pub fn remove(root: &Path, dest: &Path, branch: &str, delete_branch: bool) -> Result<()> {
    if dest.exists() {
        git(
            root,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                dest.as_os_str(),
            ],
        )?;
    } else {
        git(root, ["worktree", "prune"])?;
    }
    if delete_branch {
        git(root, ["branch", "-q", "-D", branch])?;
    }
    Ok(())
}

fn head(dir: &Path) -> Result<String> {
    Ok(git(dir, ["rev-parse", "HEAD"])?.trim().to_string())
}

/// A git config value, or `None` when it is not set.
fn config(dir: &Path, key: &str) -> Result<Option<String>> {
    let out = command(dir).args(["config", "--get", key]).output()?;
    match out.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        )),
        Some(1) => Ok(None),
        _ => Err(git_error(["config", "--get", key], &out.stderr)),
    }
}

/// `git -C <dir>` with no inherited repo overrides.
fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

fn git<I, S>(dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S> + Clone,
    S: AsRef<OsStr>,
{
    run(command(dir), args)
}

fn run<I, S>(mut cmd: Command, args: I) -> Result<String>
where
    I: IntoIterator<Item = S> + Clone,
    S: AsRef<OsStr>,
{
    let out = cmd.args(args.clone()).output()?;
    if !out.status.success() {
        return Err(git_error(args, &out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_error<I, S>(args: I, stderr: &[u8]) -> WorktreeError
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    WorktreeError::Git {
        args: args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect(),
        stderr: String::from_utf8_lossy(stderr).trim().to_string(),
    }
}

/// A throwaway copy of a worktree's index, removed on drop.
struct TempIndex {
    dir: PathBuf,
    path: PathBuf,
}

impl TempIndex {
    fn new(dir: &Path) -> Result<TempIndex> {
        let real = PathBuf::from(git(dir, ["rev-parse", "--git-path", "index"])?.trim());
        let real = if real.is_absolute() {
            real
        } else {
            dir.join(real)
        };
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut name = real.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".crew-{}-{nanos}", std::process::id()));
        let path = real.with_file_name(name);
        if real.exists() {
            fs::copy(&real, &path)?;
        }
        Ok(TempIndex {
            dir: dir.to_path_buf(),
            path,
        })
    }

    fn git<const N: usize>(&self, args: [&str; N]) -> Result<String> {
        let mut cmd = command(&self.dir);
        cmd.env("GIT_INDEX_FILE", &self.path);
        run(cmd, args)
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
