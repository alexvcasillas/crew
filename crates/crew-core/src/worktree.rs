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
/// unstaged edits, and untracked files that are not ignored. Paths in
/// `exclude` (relative to `dest`) are left out. The real index is left
/// untouched.
pub fn diff(dest: &Path, base: &str, exclude: &[String]) -> Result<Diff> {
    diff_scoped(dest, base, exclude, None, "")
}

/// Like [`diff`], narrowed to the paths in `only` when set. File paths stay
/// relative to `dest`; the patch names them under `prefix` (such as
/// `api/`), so patches of several repos read as one.
pub fn diff_scoped(
    dest: &Path,
    base: &str,
    exclude: &[String],
    only: Option<&[String]>,
    prefix: &str,
) -> Result<Diff> {
    if only.is_some_and(<[String]>::is_empty) {
        return Ok(Diff::default());
    }
    let index = TempIndex::new(dest)?;
    index.git(["add", "-A"])?;
    let mut paths = match only {
        Some(only) => only.iter().map(|p| format!(":(literal){p}")).collect(),
        None => vec![".".to_string()],
    };
    paths.extend(exclude.iter().map(|p| format!(":(exclude,literal){p}")));
    let src = format!("--src-prefix=a/{prefix}");
    let dst = format!("--dst-prefix=b/{prefix}");
    // Paths relative to `dest`, which may be a folder inside its repo.
    let diff = |opts: &[&str]| {
        let mut args = vec!["diff", "--cached", "--no-renames", "--relative"];
        args.extend_from_slice(opts);
        args.extend([base, "--"]);
        args.extend(paths.iter().map(String::as_str));
        index.git(args)
    };
    let names = diff(&["-z", "--name-status"])?;
    let counts = diff(&["-z", "--numstat"])?;
    let patch = diff(&[src.as_str(), dst.as_str()])?;

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

/// How much a worktree changed, for a summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeCounts {
    /// Files that differ from `HEAD`: uncommitted edits and new files.
    pub uncommitted: u32,
    /// Whether the branch has commits beyond the base.
    pub committed: bool,
    /// Files that differ from the base, commits and uncommitted edits
    /// together.
    pub files: u32,
    /// Lines added against the base. Binary files count 0.
    pub added: u32,
    /// Lines removed against the base. Binary files count 0.
    pub removed: u32,
}

/// Counts what changed in `dest` since `base`, the way [`diff`] sees it,
/// without building a patch. Paths in `exclude` are left out.
pub fn change_counts(dest: &Path, base: &str, exclude: &[String]) -> Result<ChangeCounts> {
    let index = TempIndex::new(dest)?;
    index.git(["add", "-A"])?;
    let mut paths = vec![".".to_string()];
    paths.extend(exclude.iter().map(|p| format!(":(exclude,literal){p}")));
    let numstat = |against: &str| {
        let mut args = vec![
            "diff",
            "--cached",
            "--no-renames",
            "-z",
            "--numstat",
            against,
        ];
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
        index.git(args)
    };
    let mut counts = ChangeCounts::default();
    for entry in numstat(base)?.split('\0').filter(|s| !s.is_empty()) {
        let mut parts = entry.splitn(3, '\t');
        let (Some(added), Some(removed)) = (parts.next(), parts.next()) else {
            continue;
        };
        counts.files += 1;
        counts.added += added.parse().unwrap_or(0);
        counts.removed += removed.parse().unwrap_or(0);
    }
    let head = head(dest)?;
    counts.committed = head != base;
    counts.uncommitted = if counts.committed {
        let changed = numstat("HEAD")?;
        changed.split('\0').filter(|s| !s.is_empty()).count() as u32
    } else {
        counts.files
    };
    Ok(counts)
}

/// Stages everything in `dest` except the paths in `exclude` (relative to
/// `dest`) and commits it. Uses the repo's git identity, or
/// `Crew <crew@localhost>` for any part that is not set. Returns the sha.
pub fn commit(dest: &Path, message: &str, exclude: &[String]) -> Result<String> {
    git(dest, ["add", "-A"])?;
    let present: Vec<String> = exclude
        .iter()
        .filter(|p| dest.join(p).exists())
        .map(|p| format!(":(literal){p}"))
        .collect();
    if !present.is_empty() {
        let mut args = vec!["reset".to_string(), "-q".into(), "--".into()];
        args.extend(present);
        git(dest, &args)?;
    }
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

/// Commits only `paths` (relative to `dest`) on the current branch,
/// leaving anything else you staged or changed as it was. New files among
/// them are added. Returns the sha.
pub fn commit_only(dest: &Path, message: &str, paths: &[String]) -> Result<String> {
    if paths.is_empty() {
        return Err(WorktreeError::BadPath(String::new()));
    }
    if let Some(bad) = paths.iter().find(|f| !is_plain_relative(f)) {
        return Err(WorktreeError::BadPath(bad.clone()));
    }
    let literal: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    // What you had staged among them stays staged if the commit fails.
    let mut args = vec![
        "diff".to_string(),
        "--cached".into(),
        "--name-only".into(),
        "-z".into(),
        "--relative".into(),
        "--".into(),
    ];
    args.extend(literal.iter().cloned());
    let staged: Vec<String> = git(dest, &args)?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    let mut args = vec!["add".to_string(), "-A".into(), "--".into()];
    args.extend(literal.iter().cloned());
    git(dest, &args)?;
    let unstage = || {
        let ours: Vec<String> = paths
            .iter()
            .filter(|p| !staged.contains(p))
            .map(|p| format!(":(literal){p}"))
            .collect();
        if ours.is_empty() {
            return;
        }
        let mut args = vec!["reset".to_string(), "-q".into(), "--".into()];
        args.extend(ours);
        let _ = git(dest, &args);
    };
    let mut args: Vec<String> = Vec::new();
    for (key, fallback) in [("user.name", "Crew"), ("user.email", "crew@localhost")] {
        if config(dest, key)?.is_none() {
            args.extend(["-c".into(), format!("{key}={fallback}")]);
        }
    }
    args.extend(["commit", "-q", "-m", message, "--only", "--"].map(String::from));
    args.extend(literal);
    if let Err(e) = git(dest, &args) {
        unstage();
        return Err(e);
    }
    head(dest)
}

/// What [`restore_tracked`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restored {
    /// Put back as they are in `HEAD`.
    pub restored: Vec<String>,
    /// Left alone: not in `HEAD`, so git can't put them back.
    pub kept: Vec<String>,
}

/// Puts each of `paths` (relative to `dest`) back as it is in `HEAD`, in
/// the index and on disk. Only files `HEAD` has are touched; new files
/// are listed as kept and never deleted.
pub fn restore_tracked(dest: &Path, paths: &[String]) -> Result<Restored> {
    if let Some(bad) = paths.iter().find(|f| !is_plain_relative(f)) {
        return Err(WorktreeError::BadPath(bad.clone()));
    }
    let mut out = Restored::default();
    for path in paths {
        let spec = format!("HEAD:./{path}");
        let tracked = command(dest)
            .args(["cat-file", "-e", spec.as_str()])
            .output()?
            .status
            .success();
        if tracked {
            out.restored.push(path.clone());
        } else {
            out.kept.push(path.clone());
        }
    }
    if !out.restored.is_empty() {
        let mut args = vec![
            "restore".to_string(),
            "--source=HEAD".into(),
            "--staged".into(),
            "--worktree".into(),
            "--".into(),
        ];
        args.extend(out.restored.iter().map(|p| format!(":(literal){p}")));
        git(dest, &args)?;
    }
    Ok(out)
}

/// The blob id of each of `paths` (relative to `dir`) as it is on disk,
/// `None` for one that is missing. With `write`, the blobs are saved in
/// the repo, so their content can be put back later.
pub fn hash_files(dir: &Path, paths: &[String], write: bool) -> Result<Vec<Option<String>>> {
    let present: Vec<&String> = paths.iter().filter(|p| dir.join(p).is_file()).collect();
    let mut ids = Vec::new();
    if !present.is_empty() {
        let mut cmd = command(dir);
        cmd.args(["hash-object", "--no-filters", "--stdin-paths"]);
        if write {
            cmd.arg("-w");
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn()?;
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().expect("piped stdin");
            for p in &present {
                writeln!(stdin, "{p}")?;
            }
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(git_error(["hash-object", "--stdin-paths"], &out.stderr));
        }
        ids = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
    }
    let mut ids = ids.into_iter();
    Ok(paths
        .iter()
        .map(|p| {
            if dir.join(p).is_file() {
                ids.next()
            } else {
                None
            }
        })
        .collect())
}

/// The blob id of `path` (relative to `dir`) in `HEAD`, if it is there.
pub fn head_blob(dir: &Path, path: &str) -> Option<String> {
    blob_at(dir, "HEAD", path)
}

/// The blob id of `path` (relative to `dir`) in commit `rev`, if it is
/// there.
pub fn blob_at(dir: &Path, rev: &str, path: &str) -> Option<String> {
    let spec = format!("{rev}:./{path}");
    git(dir, ["rev-parse", "-q", "--verify", spec.as_str()])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Writes the content of `blob` to `path` (relative to `dir`). The index
/// is left as it is.
pub fn write_blob(dir: &Path, path: &str, blob: &str) -> Result<()> {
    if !is_plain_relative(path) {
        return Err(WorktreeError::BadPath(path.to_string()));
    }
    let out = command(dir).args(["cat-file", "blob", blob]).output()?;
    if !out.status.success() {
        return Err(git_error(["cat-file", "blob", blob], &out.stderr));
    }
    let to = dir.join(path);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(to, out.stdout)?;
    Ok(())
}

/// Puts `path` (relative to `dir`) back as `HEAD` has it on disk, and
/// unstages it.
pub fn restore_from_head(dir: &Path, path: &str) -> Result<()> {
    if !is_plain_relative(path) {
        return Err(WorktreeError::BadPath(path.to_string()));
    }
    let spec = format!(":(literal){path}");
    git(
        dir,
        [
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "--",
            spec.as_str(),
        ],
    )?;
    Ok(())
}

/// Files in the checkout at `dir` that differ from `HEAD`, relative to
/// `dir`, new files one by one.
pub fn changed_files(dir: &Path) -> Result<Vec<String>> {
    let prefix = show_prefix(dir)?;
    Ok(changed_paths(dir)?
        .into_iter()
        .filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
        .collect())
}

/// Whether git ignores `path` (relative to `dir`).
pub fn is_ignored(dir: &Path, path: &str) -> bool {
    command(dir)
        .args(["check-ignore", "-q", "--", path])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The checked out branch, or `None` if detached or not a checkout.
pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, ["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}

/// The sha `HEAD` points at.
pub fn head_sha(dir: &Path) -> Result<String> {
    head(dir)
}

/// Files and folders right inside `dir` that git ignores, folders with a
/// trailing `/`. Nothing deeper is read.
pub fn ignored_top_level(dir: &Path) -> Result<Vec<String>> {
    let out = git(
        dir,
        [
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
        ],
    )?;
    let top: Vec<&str> = out
        .split('\0')
        .filter(|p| !p.is_empty())
        .filter(|p| !p.trim_end_matches('/').contains('/'))
        .collect();
    // A folder that only holds ignored files is listed too: keep those
    // that are ignored themselves.
    let dirs: Vec<&str> = top.iter().copied().filter(|p| p.ends_with('/')).collect();
    let ignored_dirs: Vec<String> = if dirs.is_empty() {
        Vec::new()
    } else {
        let mut args = vec!["check-ignore", "--"];
        args.extend(dirs.iter().copied());
        let out = command(dir).args(&args).output()?;
        match out.status.code() {
            Some(0) | Some(1) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim_end_matches('/').to_string())
                .collect(),
            _ => return Err(git_error(args, &out.stderr)),
        }
    };
    Ok(top
        .into_iter()
        .filter(|p| !p.ends_with('/') || ignored_dirs.iter().any(|d| d == p.trim_end_matches('/')))
        .map(str::to_string)
        .collect())
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

/// Whether `branch` has commits that `base` doesn't.
pub fn has_commits_beyond(root: &Path, branch: &str, base: &str) -> Result<bool> {
    let range = format!("{base}..refs/heads/{branch}");
    let count = git(root, ["rev-list", "--count", range.as_str()])?;
    Ok(count.trim() != "0")
}

/// Whether the repo at `root` has a local branch named `branch`.
pub fn branch_exists(root: &Path, branch: &str) -> Result<bool> {
    let refname = format!("refs/heads/{branch}");
    let args = ["show-ref", "--verify", "--quiet", refname.as_str()];
    let out = command(root).args(args).output()?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_error(args, &out.stderr)),
    }
}

/// The files git knows in a checkout, for finding them by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileList {
    /// Tracked files and untracked ones that are not ignored, relative to
    /// the checkout.
    pub files: Vec<String>,
    /// Those that differ from `HEAD`: staged, edited, deleted or new.
    pub changed: std::collections::HashSet<String>,
}

/// Lists the files of the checkout at `dir` with `git ls-files`, so
/// ignored folders such as `node_modules` are never read. `dir` may be a
/// folder inside a repo: only its files are listed, relative to it. Fails
/// if `dir` is not in a git checkout.
pub fn list_files(dir: &Path) -> Result<FileList> {
    let prefix = show_prefix(dir)?;
    let all = git(
        dir,
        [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut files: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in all.split('\0').filter(|p| !p.is_empty()) {
        // A file with unmerged stages is listed once per stage.
        if seen.insert(path) {
            files.push(path.to_string());
        }
    }
    let changed = changed_paths(dir)?
        .into_iter()
        .filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
        .collect();
    Ok(FileList { files, changed })
}

/// Where `dir` sits in its repo, such as `app/` or empty at the top.
fn show_prefix(dir: &Path) -> Result<String> {
    Ok(git(dir, ["rev-parse", "--show-prefix"])?.trim().to_string())
}

/// Paths under `dir` that differ from `HEAD`, relative to the repo's top,
/// new files one by one.
fn changed_paths(dir: &Path) -> Result<Vec<String>> {
    let status = git(
        dir,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--",
            ".",
        ],
    )?;
    Ok(status
        .split('\0')
        .filter(|e| e.len() > 3)
        .map(|e| e[3..].to_string())
        .collect())
}

/// What a project's own checkout looks like.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RootInfo {
    /// Whether the folder exists. Nothing else is read when it doesn't.
    pub exists: bool,
    /// Whether it is in a git checkout. When not, the rest stays empty.
    #[serde(default = "yes")]
    pub is_repo: bool,
    /// The checked out branch. `None` if detached.
    pub branch: Option<String>,
    /// Files under the folder with uncommitted changes, each new file
    /// counted.
    pub uncommitted: u32,
    /// Where `origin` points, if there is one.
    pub remote_url: Option<String>,
}

fn yes() -> bool {
    true
}

/// Reads the root's branch, uncommitted files and `origin`. Git failures
/// leave those parts empty.
pub fn root_info(root: &Path) -> RootInfo {
    if !root.is_dir() {
        return RootInfo::default();
    }
    if show_prefix(root).is_err() {
        return RootInfo {
            exists: true,
            ..RootInfo::default()
        };
    }
    let branch = current_branch(root);
    let uncommitted = changed_paths(root).map_or(0, |p| p.len());
    let remote_url = git(root, ["remote", "get-url", "origin"])
        .ok()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    RootInfo {
        exists: true,
        is_repo: true,
        branch,
        uncommitted: u32::try_from(uncommitted).unwrap_or(u32::MAX),
        remote_url,
    }
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

    fn git<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S> + Clone,
        S: AsRef<OsStr>,
    {
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
