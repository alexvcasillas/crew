//! Projects that aren't one git repo, and agents that work in the project
//! folder itself.
//!
//! A thread's changes are grouped per repo. In a copy per thread they are
//! whatever differs in each repo's worktree. In place, review shows every
//! change git sees in the repos and marks those the thread's runs didn't
//! touch (by their tool calls); commit and discard only take the touched
//! ones. Discard puts each file back as it was before the run that changed
//! it, from a snapshot, and only while it still is what that run left, so
//! your own edits survive.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crew_core::edits::edited_paths;
use crew_core::ids::{ProjectId, RunId, ThreadId};
use crew_core::project_kind::{
    ProjectKind, WorkMode, detect_kind, looks_secret, relative_to, repo_of,
};
use crew_core::scheduler::Job;
use crew_core::snapshot::{self, Before, Copy, RunSnapshot, Undo, plan_undo};
use crew_core::store::{Author, RunFilter, StoredProject, ThreadWorktree};
use crew_core::worktree::{self, Diff};
use crew_proto::{FolderInfo, OutsideFile, RepoChanges, SetupFile, SetupFileSize, ThreadChanges};

use crate::service::{Db, Reply, Service, State, blocking, json, log, same_folder};

/// Ignored folders never offered as setup files: they are built or
/// downloaded, not set up by hand.
const NOT_SETUP: [&str; 14] = [
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    "out/",
    ".next/",
    ".turbo/",
    ".cache/",
    "vendor/",
    "coverage/",
    "__pycache__/",
    ".venv/",
    "venv/",
    ".DS_Store",
];
/// An ignored folder with more files than this isn't offered either.
const MAX_SETUP_FOLDER_FILES: u32 = 50;

/// Where a thread's changes live.
pub(crate) enum Work {
    /// Nothing to review yet.
    None,
    /// A worktree of one repo, or a folder of repo worktrees.
    Copy(ThreadWorktree),
    /// The project folder itself.
    InPlace,
}

/// The `repo/` a repo's paths are shown under, empty for the folder itself.
fn prefix(repo: &str) -> String {
    if repo.is_empty() {
        String::new()
    } else {
        format!("{repo}/")
    }
}

/// Copied files that belong to `repo`, relative to it.
fn copied_in(copied: &[String], repo: &str) -> Vec<String> {
    if repo.is_empty() {
        return copied.to_vec();
    }
    let p = prefix(repo);
    copied
        .iter()
        .filter_map(|f| f.strip_prefix(&p).map(str::to_string))
        .collect()
}

/// Adds one repo's changes to the whole.
fn push_repo(
    out: &mut ThreadChanges,
    repo: &str,
    branch: Option<String>,
    diff: Diff,
    outside_thread: Vec<String>,
) {
    let p = prefix(repo);
    for f in &diff.files {
        let mut f = f.clone();
        f.path = format!("{p}{}", f.path);
        out.files.push(f);
    }
    out.patch.push_str(&diff.patch);
    out.repos.push(RepoChanges {
        repo: repo.to_string(),
        branch,
        diff,
        outside_thread,
    });
}

/// A repo's changes in place: what the thread's runs touched and git
/// sees as changed, and the other changed files. Paths relative to the
/// repo.
pub(crate) struct RepoWork {
    pub(crate) repo: String,
    pub(crate) touched: Vec<String>,
    pub(crate) others: Vec<String>,
}

/// The thread's changes in place, per repo with any, and the touched
/// files outside any repo or ignored by git (relative to the folder).
pub(crate) struct InPlace {
    pub(crate) repos: Vec<RepoWork>,
    pub(crate) outside: Vec<String>,
}

/// Sorts the folder-relative `edits` into the folder's repos next to
/// what git sees as changed there. Reads git, so off the async threads.
pub(crate) fn in_place_changes(root: &Path, kind: &ProjectKind, edits: &[String]) -> InPlace {
    let repos = kind.repos();
    let mut by_repo: HashMap<String, Vec<String>> = HashMap::new();
    let mut outside = Vec::new();
    for path in edits {
        match repo_of(&repos, path) {
            Some((repo, inner)) => by_repo
                .entry(repo.to_string())
                .or_default()
                .push(inner.to_string()),
            None => outside.push(path.clone()),
        }
    }
    let mut out = Vec::new();
    for repo in repos {
        let dir = root.join(&repo);
        if !dir.is_dir() {
            continue;
        }
        let changed = match worktree::changed_files(&dir) {
            Ok(changed) => changed,
            Err(e) => {
                log(format!(
                    "couldn't read the changes in {}: {e}",
                    dir.display()
                ));
                continue;
            }
        };
        let edited = by_repo.remove(&repo).unwrap_or_default();
        let set: HashSet<&String> = changed.iter().collect();
        let mut touched = Vec::new();
        for path in &edited {
            if set.contains(path) {
                touched.push(path.clone());
            } else if worktree::is_ignored(&dir, path) {
                outside.push(format!("{}{path}", prefix(&repo)));
            }
        }
        let others: Vec<String> = changed
            .into_iter()
            .filter(|p| !touched.contains(p))
            .collect();
        if !touched.is_empty() || !others.is_empty() {
            out.push(RepoWork {
                repo,
                touched,
                others,
            });
        }
    }
    InPlace {
        repos: out,
        outside,
    }
}

/// Removes the repo worktrees `repos` of a thread folder, and each branch
/// unless it has commits beyond its base and `delete_branch` is off.
/// Returns the repos removed, those whose branch was kept, and the error
/// that stopped it, if any.
fn remove_repo_worktrees(
    root: &Path,
    own: &Path,
    wt: &ThreadWorktree,
    repos: &[String],
    delete_branch: bool,
) -> (Vec<String>, Vec<String>, Option<String>) {
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for repo in repos {
        let Some(r) = wt.repos.iter().find(|r| &r.repo == repo) else {
            continue;
        };
        let (src, dest) = (root.join(repo), wt.path.join(repo));
        // The repo is gone from the folder: no git to ask, so only Crew's
        // own copy of it goes.
        if !src.join(".git").exists() {
            if dest.exists()
                && dest.starts_with(own)
                && let Err(e) = std::fs::remove_dir_all(&dest)
            {
                return (removed, kept, Some(format!("{repo}: {e}")));
            }
            removed.push(repo.clone());
            continue;
        }
        // Another clone now sits at the repo's path: it doesn't know the
        // worktree, so Crew's own copy goes and the clone forgets any
        // stale worktree entry.
        if dest.exists() && !same_repo(&src, &dest) {
            if dest.starts_with(own)
                && let Err(e) = std::fs::remove_dir_all(&dest)
            {
                return (removed, kept, Some(format!("{repo}: {e}")));
            }
            let pruned = std::process::Command::new("git")
                .arg("-C")
                .arg(&src)
                .args(["worktree", "prune"])
                .output();
            if let Err(e) = pruned {
                log(format!(
                    "couldn't prune worktrees of {}: {e}",
                    src.display()
                ));
            }
            removed.push(repo.clone());
            continue;
        }
        let done = (|| {
            let keep = !delete_branch && worktree::has_commits_beyond(&src, &wt.branch, &r.base)?;
            worktree::remove(&src, &dest, &wt.branch, !keep)?;
            Ok::<_, worktree::WorktreeError>(keep)
        })();
        match done {
            Ok(keep) => {
                if keep {
                    kept.push(repo.clone());
                }
                removed.push(repo.clone());
            }
            Err(e) => return (removed, kept, Some(e.to_string())),
        }
    }
    (removed, kept, None)
}

/// Whether the worktree at `dest` belongs to the repo at `src`: both
/// share one git folder.
fn same_repo(src: &Path, dest: &Path) -> bool {
    let common = |dir: &Path| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        path.canonicalize().ok()
    };
    match (common(src), common(dest)) {
        (Some(a), Some(b)) => a == b,
        // A copy git can't read any more is not the repo's.
        (Some(_), None) => false,
        _ => true,
    }
}

/// Makes a thread folder at `path` with a worktree of each of `repos` on
/// `branch`, from each repo's checked out commit. Nothing is left behind
/// if one fails.
pub(crate) fn make_repo_worktrees(
    root: &Path,
    repos: &[String],
    path: &Path,
    branch: &str,
) -> Result<ThreadWorktree, String> {
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    let mut made = Vec::new();
    for repo in repos {
        match worktree::create(&root.join(repo), "HEAD", &path.join(repo), branch) {
            Ok(base) => made.push(crew_core::store::RepoWorktree {
                repo: repo.clone(),
                base,
            }),
            Err(e) => {
                for r in &made {
                    let _ =
                        worktree::remove(&root.join(&r.repo), &path.join(&r.repo), branch, true);
                }
                let _ = std::fs::remove_dir_all(path);
                return Err(format!("{repo}: {e}"));
            }
        }
    }
    Ok(ThreadWorktree {
        path: path.to_path_buf(),
        branch: branch.to_string(),
        base: String::new(),
        ready: false,
        repos: made,
    })
}

/// The change counts of a thread's worktree, every repo's added up.
pub(crate) fn thread_counts(
    wt: &ThreadWorktree,
    copied: &[String],
) -> Result<worktree::ChangeCounts, worktree::WorktreeError> {
    if wt.repos.is_empty() {
        return worktree::change_counts(&wt.path, &wt.base, copied);
    }
    let mut total = worktree::ChangeCounts::default();
    for r in &wt.repos {
        let dir = wt.path.join(&r.repo);
        if !dir.is_dir() {
            continue;
        }
        let c = worktree::change_counts(&dir, &r.base, &copied_in(copied, &r.repo))?;
        total.uncommitted += c.uncommitted;
        total.committed |= c.committed;
        total.files += c.files;
        total.added += c.added;
        total.removed += c.removed;
    }
    Ok(total)
}

/// The checkouts whose files search lists, each with the prefix its
/// paths get: one per repo of the folder.
pub(crate) fn project_checkouts(project: &StoredProject) -> Vec<(PathBuf, String)> {
    project
        .kind
        .repos()
        .into_iter()
        .map(|r| (project.root.join(&r), prefix(&r)))
        .collect()
}

/// The same for a thread's worktree.
pub(crate) fn worktree_checkouts(wt: &ThreadWorktree) -> Vec<(PathBuf, String)> {
    if wt.repos.is_empty() {
        return vec![(wt.path.clone(), String::new())];
    }
    wt.repos
        .iter()
        .map(|r| (wt.path.join(&r.repo), prefix(&r.repo)))
        .collect()
}

/// Files under `dir`, counting stops past `cap`.
fn count_files_upto(dir: &Path, cap: u32) -> u32 {
    let mut n = 0u32;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => todo.push(entry.path()),
                Ok(t) if t.is_file() => n += 1,
                _ => {}
            }
            if n > cap {
                return n;
            }
        }
    }
    n
}

/// The ignored files at the top of each repo, as setup files.
fn setup_candidates(root: &Path, kind: &ProjectKind) -> Vec<SetupFile> {
    let mut out = Vec::new();
    for repo in kind.repos() {
        let dir = root.join(&repo);
        let Ok(found) = worktree::ignored_top_level(&dir) else {
            continue;
        };
        let mut found: Vec<String> = found
            .into_iter()
            .filter(|f| !NOT_SETUP.contains(&f.as_str()) && !f.ends_with(".log"))
            .collect();
        found.sort();
        for name in found {
            let full = dir.join(&name);
            let size = match std::fs::metadata(&full) {
                Ok(meta) if meta.is_file() => SetupFileSize::File { bytes: meta.len() },
                Ok(meta) if meta.is_dir() => {
                    let files = count_files_upto(&full, MAX_SETUP_FOLDER_FILES);
                    if files > MAX_SETUP_FOLDER_FILES {
                        continue;
                    }
                    SetupFileSize::Folder { files }
                }
                _ => continue,
            };
            let path = format!("{}{name}", prefix(&repo));
            out.push(SetupFile {
                looks_secret: looks_secret(&path),
                path,
                size,
                ignored: true,
            });
        }
    }
    out
}

/// A note naming files, `as it is` for one and `as they are` for more.
fn left_note(paths: &[String]) -> String {
    let what = if paths.len() == 1 {
        "as it is"
    } else {
        "as they are"
    };
    format!(
        "Left {} {what}: git has no earlier version to put back.",
        paths.join(", ")
    )
}

/// The top folder of the git checkout `dir` is in.
fn repo_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !top.is_empty()).then(|| PathBuf::from(top))
}

impl Service {
    /// Answers `InspectFolder`, off the async threads.
    pub(crate) async fn inspect_folder(&self, path: String) -> Reply {
        let projects = self.db().store.projects().map_err(|e| e.to_string())?;
        let info = blocking(move || {
            let picked = PathBuf::from(&path);
            if !picked.is_dir() {
                return Ok::<_, String>(FolderInfo::default());
            }
            let kind = detect_kind(&picked);
            // A folder inside a repo stands for the whole repo.
            let root = match kind {
                ProjectKind::Repo if !picked.join(".git").exists() => {
                    repo_toplevel(&picked).unwrap_or(picked)
                }
                _ => picked,
            };
            let (branch, remote) = if kind == ProjectKind::Repo {
                let info = worktree::root_info(&root);
                (info.branch, info.remote_url)
            } else {
                (None, None)
            };
            let wanted = same_folder(&root);
            Ok(FolderInfo {
                exists: true,
                root: root.to_string_lossy().into_owned(),
                name_suggestion: same_folder(&root)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                branch,
                remote,
                setup_files: setup_candidates(&root, &kind),
                already_added: projects
                    .iter()
                    .find(|p| same_folder(&p.root) == wanted)
                    .map(|p| p.id),
                kind,
            })
        })
        .await?;
        json(info)
    }

    /// Looks at the project's folder again and saves what it holds if
    /// that changed, telling clients. A folder that is gone is left as it
    /// was saved. Reads the disk, so off the async threads.
    pub(crate) fn refresh_kind(&self, project: ProjectId) -> Option<ProjectKind> {
        let stored = self.db().store.project(project).ok()?;
        if !stored.root.is_dir() {
            return Some(stored.kind);
        }
        let kind = detect_kind(&stored.root);
        if kind != stored.kind {
            let mut db = self.db();
            if let Err(e) = db.store.set_project_kind(project, &kind) {
                log(format!("couldn't save what {} holds: {e}", stored.name));
                return Some(stored.kind);
            }
            if let Err(e) = self.project_changed(&mut db, project) {
                log(e);
            }
        }
        Some(kind)
    }

    /// Refreshes what each project's folder holds. Run once at start.
    pub fn refresh_kinds(&self) {
        let projects = self.db().store.projects().unwrap_or_default();
        for p in projects {
            self.refresh_kind(p.id);
        }
    }

    /// The folder-relative files each run of the thread edited or wrote,
    /// oldest run first.
    fn run_edits(
        &self,
        thread: ThreadId,
        root: &Path,
    ) -> Result<Vec<(RunId, Vec<String>)>, String> {
        let runs = self.with_reader(|store| {
            let runs = store.runs(RunFilter {
                status: None,
                thread: Some(thread),
            })?;
            let mut all = Vec::new();
            for run in runs {
                let events: Vec<_> = store
                    .events(run.id, 0)?
                    .into_iter()
                    .map(|(_, e)| e)
                    .collect();
                all.push((run.id, events));
            }
            Ok::<_, crew_core::store::StoreError>(all)
        });
        let runs = runs.map_err(|e| e.to_string())?;
        Ok(runs
            .into_iter()
            .map(|(run, events)| {
                let mut paths: Vec<String> = Vec::new();
                for path in edited_paths(&events) {
                    if let Some(rel) = relative_to(root, &path)
                        && !paths.contains(&rel)
                    {
                        paths.push(rel);
                    }
                }
                (run, paths)
            })
            .collect())
    }

    /// The folder-relative files the thread's runs edited or wrote, in
    /// the order first touched.
    pub(crate) fn thread_edits(
        &self,
        thread: ThreadId,
        root: &Path,
    ) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = Vec::new();
        for (_, paths) in self.run_edits(thread, root)? {
            for p in paths {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        Ok(out)
    }

    /// Where the thread's changes live, given its worktree, with its
    /// project.
    fn work_of(
        &self,
        thread: ThreadId,
        wt: Option<ThreadWorktree>,
    ) -> Result<(StoredProject, Work), String> {
        let project = self
            .db()
            .store
            .project_of_thread(thread)
            .map_err(|e| e.to_string())?;
        let work = match wt {
            Some(wt) => Work::Copy(wt),
            None if project.work_mode == WorkMode::InPlace => Work::InPlace,
            None => Work::None,
        };
        Ok((project, work))
    }

    /// Answers `ThreadDiff`: the thread's uncommitted changes per repo.
    /// Files copied in from the project folder are left out.
    pub(crate) async fn thread_changes(&self, thread: ThreadId) -> Reply {
        let wt = self
            .db()
            .store
            .thread(thread)
            .map_err(|e| e.to_string())?
            .worktree;
        let (project, work) = self.work_of(thread, wt)?;
        let edits = match work {
            Work::InPlace => self.thread_edits(thread, &project.root)?,
            _ => Vec::new(),
        };
        let changes = blocking(move || {
            let mut out = ThreadChanges::default();
            match work {
                Work::None => {}
                Work::Copy(wt) if wt.repos.is_empty() => {
                    let diff = worktree::diff(&wt.path, "HEAD", &project.copy_files)?;
                    push_repo(&mut out, "", Some(wt.branch.clone()), diff, Vec::new());
                }
                Work::Copy(wt) => {
                    for r in &wt.repos {
                        let dir = wt.path.join(&r.repo);
                        // A repo gone from the folder leaves a worktree git can't read.
                        if !dir.is_dir() || !project.root.join(&r.repo).is_dir() {
                            continue;
                        }
                        let copied = copied_in(&project.copy_files, &r.repo);
                        let diff =
                            worktree::diff_scoped(&dir, "HEAD", &copied, None, &prefix(&r.repo))?;
                        push_repo(&mut out, &r.repo, Some(wt.branch.clone()), diff, Vec::new());
                    }
                }
                Work::InPlace => {
                    let found = in_place_changes(&project.root, &project.kind, &edits);
                    for work in found.repos {
                        let dir = project.root.join(&work.repo);
                        let diff =
                            worktree::diff_scoped(&dir, "HEAD", &[], None, &prefix(&work.repo))?;
                        let branch = worktree::current_branch(&dir);
                        push_repo(&mut out, &work.repo, branch, diff, work.others);
                    }
                    out.outside_git = found
                        .outside
                        .into_iter()
                        .map(|path| OutsideFile {
                            exists: project.root.join(&path).exists(),
                            path,
                        })
                        .collect();
                }
            }
            Ok::<_, worktree::WorktreeError>(out)
        })
        .await?;
        json(changes)
    }

    /// Holds the project folder for a commit or discard in place: no
    /// agent writes there meanwhile. Refused while one does.
    fn claim_folder(&self, project: ProjectId) -> Result<(), String> {
        let mut state = self.state();
        if let Some(run) = state.writers.get(&project).copied() {
            let agent = self.db().store.run(run).map(|r| r.agent);
            let name = match agent {
                Ok(agent) => self.agent_name(agent),
                Err(_) => "the agent".to_string(),
            };
            return Err(format!("Wait for {name} to finish writing, or stop it."));
        }
        if !state.changing.insert(project) {
            return Err("Another change to this folder is in progress.".into());
        }
        Ok(())
    }

    /// Takes the thread, and for work in place its project folder, so
    /// nothing runs there while its changes are committed or dropped.
    /// Returns the project and where the thread's changes live.
    fn claim_work(
        &self,
        thread: ThreadId,
        busy: impl FnOnce(&str) -> String,
    ) -> Result<(StoredProject, Work), String> {
        let wt = self.claim_for_change(thread, busy)?;
        let found = self.work_of(thread, wt).and_then(|(project, work)| {
            if matches!(work, Work::InPlace) {
                self.claim_folder(project.id)?;
            }
            Ok((project, work))
        });
        if found.is_err() {
            self.release_change(thread);
        }
        found
    }

    /// Lets runs start in the thread and its folder again after a commit
    /// or discard, and drops what may have changed.
    fn after_change(&self, thread: ThreadId, project: &StoredProject, work: &Work) {
        self.changes_may_differ(thread);
        if matches!(work, Work::InPlace) {
            self.project_files_may_differ(project);
            self.state().changing.remove(&project.id);
        }
        self.release_change(thread);
    }

    /// Answers `Commit`: commits the thread's changes in one repo.
    pub(crate) async fn commit_in(
        &self,
        thread: ThreadId,
        repo: Option<String>,
        message: &str,
    ) -> Reply {
        let message = message.trim().to_string();
        if message.is_empty() {
            return Err("Write a commit message.".into());
        }
        let (project, work) = self.claim_work(thread, |name| {
            format!("Wait for {name} to finish, or stop it.")
        })?;
        let result = self
            .commit_claimed(thread, &project, &work, repo, message)
            .await;
        self.after_change(thread, &project, &work);
        if matches!(result, Ok(Some(_))) {
            self.thread_overview_stale(thread);
        }
        match result? {
            Some(sha) => json(sha),
            None => Err("Nothing to commit.".into()),
        }
    }

    async fn commit_claimed(
        &self,
        thread: ThreadId,
        project: &StoredProject,
        work: &Work,
        repo: Option<String>,
        message: String,
    ) -> Result<Option<String>, String> {
        let repo = repo.filter(|r| !r.is_empty());
        // Which folder to commit in, and what: everything but the copied
        // files, or only the thread's paths.
        let (dir, only, copied) = match work {
            Work::None => return Err("This thread has no worktree.".into()),
            Work::Copy(wt) if wt.repos.is_empty() => {
                if repo.is_some() {
                    return Err("This thread has one repo; commit without naming one.".into());
                }
                (wt.path.clone(), None, project.copy_files.clone())
            }
            Work::Copy(wt) => {
                let repo = repo.ok_or("Pick a repo to commit in.")?;
                if !wt.repos.iter().any(|r| r.repo == repo) {
                    return Err(format!("{repo} isn't a repo of this thread."));
                }
                let copied = copied_in(&project.copy_files, &repo);
                (wt.path.join(&repo), None, copied)
            }
            Work::InPlace => {
                let repos = project.kind.repos();
                if repos.is_empty() {
                    return Err("Nothing in this folder is in git.".into());
                }
                let repo = match repo {
                    Some(repo) => repo,
                    None if repos == [String::new()] => String::new(),
                    None => return Err("Pick a repo to commit in.".into()),
                };
                if !repos.contains(&repo) {
                    return Err(format!("{repo} isn't a repo of this folder."));
                }
                (project.root.join(&repo), Some(repo), Vec::new())
            }
        };
        let edits = match work {
            Work::InPlace => self.run_edits(thread, &project.root)?,
            _ => Vec::new(),
        };
        let (root, kind) = (project.root.clone(), project.kind.clone());
        let snapshots = self.snapshots_dir();
        blocking(move || {
            let Some(repo) = only else {
                if worktree::diff(&dir, "HEAD", &copied)?.files.is_empty() {
                    return Ok(None);
                }
                return worktree::commit(&dir, &message, &copied).map(Some);
            };
            let all: Vec<String> = edits.iter().flat_map(|(_, p)| p.clone()).collect();
            let found = in_place_changes(&root, &kind, &all);
            let paths = found
                .repos
                .into_iter()
                .find(|w| w.repo == repo)
                .map(|w| w.touched)
                .unwrap_or_default();
            if paths.is_empty() {
                return Ok(None);
            }
            let sha = worktree::commit_only(&dir, &message, &paths)?;
            // Committed work is no longer the runs' to undo.
            let done: Vec<String> = paths
                .iter()
                .map(|p| format!("{}{p}", prefix(&repo)))
                .collect();
            forget_in_snapshots(&snapshots, &edits, &done);
            Ok::<_, worktree::WorktreeError>(Some(sha))
        })
        .await
    }

    /// Answers `Discard`.
    pub(crate) async fn discard_in(
        &self,
        thread: ThreadId,
        repos: Option<Vec<String>>,
        delete_branch: bool,
    ) -> Reply {
        let (project, work) = self.claim_work(thread, |name| format!("Stop {name} first."))?;
        let result = match &work {
            Work::None => Err("This thread has no worktree.".to_string()),
            Work::Copy(wt) if wt.repos.is_empty() => {
                self.discard_claimed(thread, wt.clone(), delete_branch)
                    .await
            }
            Work::Copy(wt) => {
                self.discard_repos(thread, &project, wt, repos, delete_branch)
                    .await
            }
            Work::InPlace => self.discard_in_place(thread, &project, repos).await,
        };
        self.after_change(thread, &project, &work);
        self.thread_overview_stale(thread);
        result.map(|()| serde_json::Value::Null)
    }

    /// Removes the chosen repo worktrees of a thread folder, and the
    /// folder once none is left.
    async fn discard_repos(
        &self,
        thread: ThreadId,
        project: &StoredProject,
        wt: &ThreadWorktree,
        repos: Option<Vec<String>>,
        delete_branch: bool,
    ) -> Result<(), String> {
        let all: Vec<String> = wt.repos.iter().map(|r| r.repo.clone()).collect();
        let chosen = repos.unwrap_or_else(|| all.clone());
        if let Some(bad) = chosen.iter().find(|r| !all.contains(r)) {
            return Err(format!("{bad} isn't a repo of this thread."));
        }
        let (root, own) = (project.root.clone(), self.worktrees().to_path_buf());
        let held = wt.clone();
        let (removed, kept, failed, gone) = blocking(move || {
            let (removed, kept, failed) =
                remove_repo_worktrees(&root, &own, &held, &chosen, delete_branch);
            let left = held.repos.iter().any(|r| !removed.contains(&r.repo));
            // The thread folder is Crew's own; it goes once empty of repos.
            let mut gone = false;
            if !left && held.path.starts_with(&own) {
                if held.path.exists()
                    && let Err(e) = std::fs::remove_dir_all(&held.path)
                {
                    log(format!("couldn't remove {}: {e}", held.path.display()));
                }
                gone = true;
            }
            Ok::<_, String>((removed, kept, failed, gone))
        })
        .await?;
        let saved = {
            let mut db = self.db();
            let left = ThreadWorktree {
                repos: wt
                    .repos
                    .iter()
                    .filter(|r| !removed.contains(&r.repo))
                    .cloned()
                    .collect(),
                ..wt.clone()
            };
            let next = (!gone && !left.repos.is_empty()).then_some(&left);
            let mut saved = db.store.set_thread_worktree(thread, next);
            if saved.is_ok() {
                self.thread_changed_or_log(&mut db, thread);
            }
            for repo in &kept {
                let body = format!(
                    "Kept branch {} in {repo} because it has commits that aren't merged.",
                    wt.branch
                );
                if saved.is_ok() {
                    saved = self
                        .add_message(&mut db, thread, Author::System, &body, None)
                        .map(|_| ());
                }
            }
            saved.map_err(|e| e.to_string())
        };
        if let Some(e) = failed {
            return Err(e);
        }
        saved
    }

    /// Undoes the thread's runs in the project folder, file by file, from
    /// their snapshots. A file is put back only while it still is what the
    /// last run that touched it left; your edits made before the run come
    /// back with it. Files git doesn't have are never touched.
    async fn discard_in_place(
        &self,
        thread: ThreadId,
        project: &StoredProject,
        repos: Option<Vec<String>>,
    ) -> Result<(), String> {
        let all = project.kind.repos();
        let chosen = repos.unwrap_or_else(|| all.clone());
        if let Some(bad) = chosen.iter().find(|r| !all.contains(r)) {
            return Err(format!("{bad} isn't a repo of this folder."));
        }
        let edits = self.run_edits(thread, &project.root)?;
        let (root, kind) = (project.root.clone(), project.kind.clone());
        let dir_of_snapshots = self.snapshots_dir();
        let store = blob_store(&dir_of_snapshots);
        let (notes, failed) = blocking(move || {
            let snaps: HashMap<RunId, RunSnapshot> = edits
                .iter()
                .filter_map(|(run, _)| Some((*run, load_snapshot(&dir_of_snapshots, *run)?)))
                .collect();
            let every: Vec<String> = edits.iter().flat_map(|(_, p)| p.clone()).collect();
            let found = in_place_changes(&root, &kind, &every);
            let mut notes = Vec::new();
            let mut left_new = Vec::new();
            let mut undone = Vec::new();
            let mut failed = None;
            for work in found.repos {
                if !chosen.contains(&work.repo) {
                    continue;
                }
                let dir = root.join(&work.repo);
                for path in &work.touched {
                    let full = format!("{}{path}", prefix(&work.repo));
                    let runs: Vec<(Option<Before>, Option<Option<String>>)> = edits
                        .iter()
                        .rev()
                        .filter(|(_, paths)| paths.contains(&full))
                        .map(|(run, _)| match snaps.get(run) {
                            Some(snap) => {
                                let before = snap.before(&work.repo, path).map(|b| match b {
                                    // Clean then, but only if the start HEAD had it.
                                    Before::Head => {
                                        let head = snap
                                            .repos
                                            .iter()
                                            .find(|r| r.repo == work.repo)
                                            .map(|r| r.head.clone())
                                            .unwrap_or_default();
                                        if worktree::blob_at(&dir, &head, path).is_some() {
                                            Before::Head
                                        } else {
                                            Before::Absent
                                        }
                                    }
                                    other => other,
                                });
                                (before, snap.end_of(&full))
                            }
                            None => (None, None),
                        })
                        .collect();
                    let current = worktree::hash_files(&dir, std::slice::from_ref(path), false)
                        .ok()
                        .and_then(|mut v| v.pop())
                        .flatten();
                    let head = worktree::head_blob(&dir, path);
                    let done = match plan_undo(current.as_deref(), head.as_deref(), &runs) {
                        Undo::To(Before::Content(kept)) => {
                            let copy = match kept.copy {
                                Copy::TooBig => {
                                    notes.push(format!(
                                        "{full} was too big to keep a copy, left as is."
                                    ));
                                    continue;
                                }
                                Copy::Failed => {
                                    notes.push(format!(
                                        "Crew couldn't copy {full} before the run, left as is."
                                    ));
                                    continue;
                                }
                                Copy::Kept => snapshot::copy_path(&store, &kept.blob),
                            };
                            let Some(copy) = copy.filter(|c| c.is_file()) else {
                                notes.push(format!("{full}'s copy is missing, left as is."));
                                continue;
                            };
                            // The copy must still be what was kept.
                            if snapshot::id_of(&dir, &copy).ok().as_deref()
                                != Some(kept.blob.as_str())
                            {
                                notes.push(format!("{full}'s copy is damaged, left as is."));
                                continue;
                            }
                            put_back(&dir, path, &copy, kept.mode)
                        }
                        Undo::To(Before::Head) => worktree::restore_from_head(&dir, path),
                        Undo::To(Before::Absent) => {
                            left_new.push(full.clone());
                            continue;
                        }
                        Undo::ChangedAfter => {
                            notes.push(format!("{full} changed after the run, left as is."));
                            continue;
                        }
                        Undo::NoRecord => {
                            notes.push(format!(
                                "{full} has no record from before the run, left as is."
                            ));
                            continue;
                        }
                    };
                    match done {
                        Ok(()) => undone.push(full),
                        Err(e) => {
                            failed.get_or_insert_with(|| format!("Couldn't put back {full}: {e}"));
                        }
                    }
                }
            }
            forget_in_snapshots(&dir_of_snapshots, &edits, &undone);
            if !left_new.is_empty() {
                notes.insert(0, left_note(&left_new));
            }
            Ok::<_, String>((notes, failed))
        })
        .await?;
        {
            let mut db = self.db();
            for body in notes {
                if let Err(e) = self.add_message(&mut db, thread, Author::System, &body, None) {
                    log(e);
                }
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Where run snapshots are kept: next to the worktrees.
    pub(crate) fn snapshots_dir(&self) -> PathBuf {
        let worktrees = self.worktrees();
        worktrees.parent().unwrap_or(worktrees).join(SNAPSHOTS_DIR)
    }

    /// Notes what the project's repos hold before an in-place run writes.
    /// A snapshot that can't be saved is logged; the run's files then
    /// can't be undone and are left as they are.
    pub(crate) async fn take_snapshot(&self, job: &Job) {
        let Ok(project) = self.db().store.project_of_thread(job.thread) else {
            return;
        };
        let (dir, run) = (self.snapshots_dir(), job.run);
        let taken = blocking(move || {
            let store = blob_store(&dir);
            let snap = snapshot::take(
                &project.root,
                &project.kind.repos(),
                &store,
                snapshot::Caps::default(),
            );
            save_snapshot(&dir, run, &snap)?;
            Ok::<_, String>(snap.skipped())
        })
        .await;
        let note = match taken {
            Ok(0) => None,
            Ok(1) => Some("Crew couldn't keep a copy of 1 file, so it won't undo it.".to_string()),
            Ok(n) => Some(format!(
                "Crew couldn't keep copies of {n} files, so it won't undo them."
            )),
            Err(e) => {
                log(format!("couldn't save the snapshot of run {run}: {e}"));
                None
            }
        };
        if let Some(body) = note {
            let mut db = self.db();
            if let Err(e) = self.add_message(&mut db, job.thread, Author::System, &body, Some(run))
            {
                log(e);
            }
        }
    }

    /// Notes what an in-place run left in each file it touched. Does
    /// nothing for a run without a snapshot. Then lists the folder's files
    /// again for search.
    pub(crate) async fn record_end(&self, thread: ThreadId, run: RunId) {
        let dir = self.snapshots_dir();
        if !snapshot_path(&dir, run).exists() {
            return;
        }
        let Ok(project) = self.db().store.project_of_thread(thread) else {
            return;
        };
        let edits = match self.run_edits(thread, &project.root) {
            Ok(edits) => edits
                .into_iter()
                .find(|(r, _)| *r == run)
                .map(|(_, p)| p)
                .unwrap_or_default(),
            Err(e) => {
                log(format!("couldn't read what run {run} changed: {e}"));
                return;
            }
        };
        let (root, kind) = (project.root.clone(), project.kind.clone());
        let saved = blocking(move || {
            let mut snap = load_snapshot(&dir, run).unwrap_or_default();
            let repos = kind.repos();
            let mut end = Vec::new();
            let mut untracked = Vec::new();
            for path in edits {
                let Some((repo, inner)) = repo_of(&repos, &path) else {
                    if through_link(&root, &repos, &path) {
                        untracked.push(path);
                    }
                    continue;
                };
                let blob = worktree::hash_files(&root.join(repo), &[inner.to_string()], false)
                    .map_err(|e| e.to_string())?
                    .pop()
                    .flatten();
                end.push((path.clone(), blob));
            }
            if end.is_empty() {
                // Nothing of the run to undo.
                forget_run(&dir, run);
            } else {
                snap.end = Some(end);
                save_snapshot(&dir, run, &snap)?;
            }
            Ok::<_, String>(untracked)
        })
        .await;
        match saved {
            Ok(untracked) if !untracked.is_empty() => {
                let mut db = self.db();
                for path in untracked {
                    let body = format!("Edits through {path} aren't tracked by Crew.");
                    if let Err(e) =
                        self.add_message(&mut db, thread, Author::System, &body, Some(run))
                    {
                        log(e);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => log(format!("couldn't save how run {run} ended: {e}")),
        }
        self.project_files_may_differ(&project);
    }

    /// Deletes run records older than [`SNAPSHOT_DAYS`] and copies no
    /// record points to. Run once at start, before any run.
    pub fn clean_snapshots(&self) {
        let max = std::time::Duration::from_secs(SNAPSHOT_DAYS * 24 * 60 * 60);
        gc_snapshots(&self.snapshots_dir(), max);
    }

    /// Drops the run records of a thread: nothing of it is left to undo,
    /// as when it is archived.
    pub(crate) fn forget_thread_snapshots(&self, thread: ThreadId) {
        let runs = self.db().store.runs(RunFilter {
            status: None,
            thread: Some(thread),
        });
        let dir = self.snapshots_dir();
        for run in runs.unwrap_or_default() {
            forget_run(&dir, run.id);
        }
    }

    /// The project whose folder `job` would write in, if its agent may
    /// write and works in place there.
    pub(crate) fn in_place_writer(&self, db: &Db, job: &Job) -> Option<ProjectId> {
        let writes = db
            .store
            .agent(job.agent)
            .is_ok_and(|a| a.spec.permission_mode.edits());
        if !writes {
            return None;
        }
        let thread = db.store.thread(job.thread).ok()?;
        if thread.worktree.is_some() {
            return None;
        }
        let project = db.store.project_of_thread(job.thread).ok()?;
        (project.work_mode == WorkMode::InPlace).then_some(project.id)
    }

    /// The writing run `job` has to wait for, if any: in place, one agent
    /// writes in a folder at a time.
    pub(crate) fn lock_holder(
        &self,
        db: &Db,
        writers: &HashMap<ProjectId, RunId>,
        job: &Job,
    ) -> Option<RunId> {
        let project = self.in_place_writer(db, job)?;
        writers.get(&project).copied().filter(|r| *r != job.run)
    }

    /// Whether `job` may start now: it doesn't write in place, or its
    /// folder has no other writer and no commit or discard under way.
    pub(crate) fn may_write(
        &self,
        db: &Db,
        writers: &HashMap<ProjectId, RunId>,
        changing: &HashSet<ProjectId>,
        job: &Job,
    ) -> bool {
        match self.in_place_writer(db, job) {
            Some(project) => {
                !changing.contains(&project) && writers.get(&project).is_none_or(|r| *r == job.run)
            }
            None => true,
        }
    }

    /// Saves which run each queued run waits for, telling the thread when
    /// that changed.
    pub(crate) fn update_waiting(&self, state: &State) {
        let mut db = self.db();
        for job in state.scheduler.queued() {
            let want = self.lock_holder(&db, &state.writers, job);
            let have = match db.store.run(job.run) {
                Ok(run) => run.waiting_for,
                Err(e) => {
                    log(format!("couldn't read run {}: {e}", job.run));
                    continue;
                }
            };
            if want == have {
                continue;
            }
            match db.store.set_run_waiting(job.run, want) {
                Ok(()) => self.publish_status(
                    &mut db,
                    job.thread,
                    job.run,
                    crew_core::event::RunStatus::Queued,
                ),
                Err(e) => log(format!("couldn't save what run {} waits for: {e}", job.run)),
            }
        }
    }
}

/// Folder in the crew home that holds run snapshots.
const SNAPSHOTS_DIR: &str = "snapshots";
/// Days a run snapshot is kept.
const SNAPSHOT_DAYS: u64 = 30;

/// Where copies are kept, by blob id, shared by all runs.
fn blob_store(dir: &Path) -> PathBuf {
    dir.join("blobs")
}

/// Where run records are kept.
fn runs_dir(dir: &Path) -> PathBuf {
    dir.join("runs")
}

fn snapshot_path(dir: &Path, run: RunId) -> PathBuf {
    runs_dir(dir).join(format!("{run}.json"))
}

fn load_snapshot(dir: &Path, run: RunId) -> Option<RunSnapshot> {
    let text = std::fs::read_to_string(snapshot_path(dir, run)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Saves the record, 0600 in a 0700 folder.
fn save_snapshot(dir: &Path, run: RunId, snap: &RunSnapshot) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    for folder in [dir.to_path_buf(), runs_dir(dir)] {
        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string(snap).map_err(|e| e.to_string())?;
    let path = snapshot_path(dir, run);
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())
}

/// Deletes a run's record. Its copies go at the next start, once no
/// record points to them.
fn forget_run(dir: &Path, run: RunId) {
    let path = snapshot_path(dir, run);
    if path.exists()
        && let Err(e) = std::fs::remove_file(&path)
    {
        log(format!("couldn't remove {}: {e}", path.display()));
    }
}

/// Deletes run records older than `max`, then every copy no record points
/// to. Anything else in `dir` is from an older layout and goes too.
pub(crate) fn gc_snapshots(dir: &Path, max: std::time::Duration) {
    let remove = |path: &Path| {
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        if let Err(e) = removed {
            log(format!("couldn't remove {}: {e}", path.display()));
        }
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Only what Crew made goes: an older layout's run folder holds its
    // `snapshot.json`. Anything else here is left alone.
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let plain_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if name != "blobs" && name != "runs" && plain_dir && path.join("snapshot.json").is_file() {
            remove(&path);
        }
    }
    let is_file = |entry: &std::fs::DirEntry| entry.file_type().is_ok_and(|t| t.is_file());
    let mut wanted: HashSet<String> = HashSet::new();
    for entry in std::fs::read_dir(runs_dir(dir))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        // Records are `<run>.json`; nothing else in `runs` is Crew's.
        if !is_file(&entry) || path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > max);
        let snap = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<RunSnapshot>(&t).ok());
        match snap {
            Some(snap) if !old => wanted.extend(snap.copies().map(str::to_string)),
            _ => remove(&path),
        }
    }
    for entry in std::fs::read_dir(blob_store(dir))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Copies are named by their id, half-made ones `.copy-…`.
        let crews = crew_core::snapshot::valid_id(&name) || name.starts_with(".copy-");
        if is_file(&entry) && crews && !wanted.contains(&name) {
            remove(&entry.path());
        }
    }
}

/// Whether the folder-relative `path`, outside every repo, leads into a
/// repo of the folder that is reached through a link.
fn through_link(root: &Path, repos: &[String], path: &str) -> bool {
    let Ok(real) = root.join(path).canonicalize() else {
        return false;
    };
    let Ok(real_root) = root.canonicalize() else {
        return false;
    };
    repos.iter().filter(|r| !r.is_empty()).any(|r| {
        root.join(r)
            .canonicalize()
            .is_ok_and(|target| target != real_root.join(r) && real.starts_with(&target))
    })
}

/// Writes the kept copy back to `path` in the repo at `repo_dir`, with
/// `mode` (else the file's own, else 0644). Never writes through a link:
/// the file must not be one, and its folder must lie inside the repo. The
/// new content is renamed into place.
pub(crate) fn put_back(
    repo_dir: &Path,
    path: &str,
    copy: &Path,
    mode: Option<u32>,
) -> Result<(), worktree::WorktreeError> {
    use std::os::unix::fs::PermissionsExt;
    let outside = || {
        worktree::WorktreeError::Io(std::io::Error::other(format!(
            "{path} leads outside the repo"
        )))
    };
    let target = repo_dir.join(path);
    let real_repo = repo_dir.canonicalize()?;
    let parent = target.parent().ok_or_else(outside)?;
    let mut probe = parent;
    while !probe.exists() {
        probe = probe.parent().ok_or_else(outside)?;
    }
    if !probe.canonicalize()?.starts_with(&real_repo) {
        return Err(outside());
    }
    std::fs::create_dir_all(parent)?;
    if !parent.canonicalize()?.starts_with(&real_repo) {
        return Err(outside());
    }
    let existing = std::fs::symlink_metadata(&target).ok();
    if existing.as_ref().is_some_and(|m| !m.is_file()) {
        return Err(worktree::WorktreeError::Io(std::io::Error::other(format!(
            "{path} is no longer a plain file"
        ))));
    }
    let mode = mode
        .or_else(|| existing.map(|m| m.permissions().mode() & 0o7777))
        .unwrap_or(0o644);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".crew-restore-{}-{nanos}", std::process::id()));
    let written = (|| {
        std::fs::copy(copy, &tmp)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, &target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

/// Drops `paths` (folder-relative) from the end records of the runs in
/// `edits`, so they are no longer the runs' to undo.
fn forget_in_snapshots(dir: &Path, edits: &[(RunId, Vec<String>)], paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    for (run, touched) in edits {
        if !touched.iter().any(|p| paths.contains(p)) {
            continue;
        }
        let Some(mut snap) = load_snapshot(dir, *run) else {
            continue;
        };
        if let Some(end) = &mut snap.end {
            end.retain(|(p, _)| !paths.contains(p));
            if end.is_empty() {
                forget_run(dir, *run);
                continue;
            }
        }
        if let Err(e) = save_snapshot(dir, *run, &snap) {
            log(format!("couldn't update the snapshot of run {run}: {e}"));
        }
    }
}

/// What the thread's runs changed in place, counted for a channel
/// overview: the touched files git sees as changed.
pub(crate) fn in_place_counts(
    root: &Path,
    kind: &ProjectKind,
    edits: &[String],
) -> Result<worktree::ChangeCounts, worktree::WorktreeError> {
    let mut total = worktree::ChangeCounts::default();
    for work in in_place_changes(root, kind, edits).repos {
        if work.touched.is_empty() {
            continue;
        }
        let dir = root.join(&work.repo);
        let diff = worktree::diff_scoped(&dir, "HEAD", &[], Some(&work.touched), "")?;
        for f in diff.files {
            total.files += 1;
            total.added += f.added;
            total.removed += f.removed;
        }
    }
    total.uncommitted = total.files;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    use crate::folders::{gc_snapshots, put_back};

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn put_back_never_writes_through_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("api");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let copy = tmp.path().join("copy");
        std::fs::write(&copy, "kept\n").unwrap();
        std::fs::write(outside.join("x.txt"), "yours\n").unwrap();
        // The file is a link now.
        std::os::unix::fs::symlink(outside.join("x.txt"), repo.join("x.txt")).unwrap();
        assert!(put_back(&repo, "x.txt", &copy, None).is_err());
        // Its folder is a link now.
        std::os::unix::fs::symlink(&outside, repo.join("sub")).unwrap();
        assert!(put_back(&repo, "sub/x.txt", &copy, None).is_err());
        assert!(put_back(&repo, "sub/new/y.txt", &copy, None).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("x.txt")).unwrap(),
            "yours\n"
        );
        assert!(!outside.join("new").exists());

        // A plain file comes back with the mode it had.
        put_back(&repo, "bin/run.sh", &copy, Some(0o755)).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("bin/run.sh")).unwrap(),
            "kept\n"
        );
        assert_eq!(mode(&repo.join("bin/run.sh")), 0o755);
        std::fs::write(repo.join("a.txt"), "agent\n").unwrap();
        std::fs::set_permissions(repo.join("a.txt"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        put_back(&repo, "a.txt", &copy, None).unwrap();
        assert_eq!(mode(&repo.join("a.txt")), 0o600);
    }

    #[test]
    fn gc_keeps_only_copies_a_record_points_to() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("snapshots");
        let (runs, blobs) = (dir.join("runs"), dir.join("blobs"));
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::create_dir_all(&blobs).unwrap();
        let (used, unused) = ("a".repeat(40), "b".repeat(40));
        std::fs::write(blobs.join(&used), "x").unwrap();
        std::fs::write(blobs.join(&unused), "y").unwrap();
        std::fs::write(blobs.join(".copy-1-2"), "half").unwrap();
        let record = serde_json::json!({"repos": [{"repo": "", "head": "h", "dirty": [
            {"path": "a.txt", "blob": used, "copy": "kept", "mode": 420}
        ]}]});
        std::fs::write(runs.join("r1.json"), record.to_string()).unwrap();
        std::fs::write(runs.join("broken.json"), "{").unwrap();
        // An older layout's folder.
        std::fs::create_dir_all(dir.join("0000/files")).unwrap();
        std::fs::write(dir.join("0000/snapshot.json"), "{}").unwrap();
        // Things Crew never makes stay, wherever they are.
        std::fs::create_dir_all(dir.join("photos")).unwrap();
        std::fs::write(dir.join("photos/cat.jpg"), "cat").unwrap();
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        std::fs::write(runs.join("notes.txt"), "mine").unwrap();
        std::fs::create_dir_all(runs.join("keep")).unwrap();
        std::fs::write(blobs.join("readme.md"), "mine").unwrap();

        gc_snapshots(&dir, Duration::from_secs(3600));
        let mut left: Vec<String> = std::fs::read_dir(&blobs)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [used.clone(), "readme.md".to_string()]);
        assert!(runs.join("r1.json").exists());
        assert!(!runs.join("broken.json").exists());
        assert!(!dir.join("0000").exists());
        assert!(dir.join("photos/cat.jpg").exists());
        assert!(dir.join("notes.txt").exists());
        assert!(runs.join("notes.txt").exists() && runs.join("keep").is_dir());

        // Past the age limit the record goes, then its copies.
        gc_snapshots(&dir, Duration::ZERO);
        assert!(!runs.join("r1.json").exists());
        assert!(!blobs.join(&used).exists());
        assert!(blobs.join("readme.md").exists());
    }
}
