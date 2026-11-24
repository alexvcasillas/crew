//! Projects that aren't one git repo, and agents that work in the project
//! folder itself.
//!
//! A thread's changes are grouped per repo. In a copy per thread they are
//! whatever differs in each repo's worktree. In place they are only the
//! files the thread's runs edited or wrote, by their tool calls, that git
//! sees as changed: your own edits in the same folder stay yours.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crew_core::edits::edited_paths;
use crew_core::ids::{ProjectId, RunId, ThreadId};
use crew_core::project_kind::{
    ProjectKind, WorkMode, detect_kind, looks_secret, relative_to, repo_of,
};
use crew_core::scheduler::Job;
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
fn push_repo(out: &mut ThreadChanges, repo: &str, branch: Option<String>, diff: Diff) {
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
    });
}

/// Paths the thread's runs changed in place, per repo (relative to it),
/// and those outside any repo or ignored by git (relative to the folder).
pub(crate) struct InPlace {
    pub(crate) repos: Vec<(String, Vec<String>)>,
    pub(crate) outside: Vec<String>,
}

/// Sorts the folder-relative `edits` into repos, keeping only what git
/// sees as changed. Reads git, so off the async threads.
pub(crate) fn in_place_changes(root: &Path, kind: &ProjectKind, edits: &[String]) -> InPlace {
    let repos = kind.repos();
    let mut by_repo: Vec<(String, Vec<String>)> = Vec::new();
    let mut outside = Vec::new();
    for path in edits {
        match repo_of(&repos, path) {
            Some((repo, inner)) => match by_repo.iter_mut().find(|(r, _)| r == repo) {
                Some((_, paths)) => paths.push(inner.to_string()),
                None => by_repo.push((repo.to_string(), vec![inner.to_string()])),
            },
            None => outside.push(path.clone()),
        }
    }
    let mut out = Vec::new();
    for (repo, paths) in by_repo {
        let dir = root.join(&repo);
        let changed: HashSet<String> = match worktree::changed_files(&dir) {
            Ok(changed) => changed.into_iter().collect(),
            Err(e) => {
                log(format!(
                    "couldn't read the changes in {}: {e}",
                    dir.display()
                ));
                continue;
            }
        };
        let mut kept = Vec::new();
        for path in paths {
            if changed.contains(&path) {
                kept.push(path);
            } else if worktree::is_ignored(&dir, &path) {
                outside.push(format!("{}{path}", prefix(&repo)));
            }
        }
        if !kept.is_empty() {
            out.push((repo, kept));
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

impl Service {
    /// Answers `InspectFolder`, off the async threads.
    pub(crate) async fn inspect_folder(&self, path: String) -> Reply {
        let projects = self.db().store.projects().map_err(|e| e.to_string())?;
        let info = blocking(move || {
            let root = PathBuf::from(&path);
            if !root.is_dir() {
                return Ok::<_, String>(FolderInfo::default());
            }
            let kind = detect_kind(&root);
            let (branch, remote) = if kind == ProjectKind::Repo {
                let info = worktree::root_info(&root);
                (info.branch, info.remote_url)
            } else {
                (None, None)
            };
            let wanted = same_folder(&root);
            Ok(FolderInfo {
                exists: true,
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

    /// The folder-relative files the thread's runs edited or wrote, in
    /// the order first touched.
    fn thread_edits(&self, thread: ThreadId, root: &Path) -> Result<Vec<String>, String> {
        let events = self.with_reader(|store| {
            let runs = store.runs(RunFilter {
                status: None,
                thread: Some(thread),
            })?;
            let mut all = Vec::new();
            for run in runs {
                all.extend(store.events(run.id, 0)?.into_iter().map(|(_, e)| e));
            }
            Ok::<_, crew_core::store::StoreError>(all)
        });
        let events = events.map_err(|e| e.to_string())?;
        let mut out: Vec<String> = Vec::new();
        for path in edited_paths(&events) {
            if let Some(rel) = relative_to(root, &path)
                && !out.contains(&rel)
            {
                out.push(rel);
            }
        }
        Ok(out)
    }

    /// Where the thread's changes live, with its project.
    fn work_of(&self, thread: ThreadId) -> Result<(StoredProject, Work), String> {
        let db = self.db();
        let project = db
            .store
            .project_of_thread(thread)
            .map_err(|e| e.to_string())?;
        let wt = db.store.thread(thread).map_err(|e| e.to_string())?.worktree;
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
        let (project, work) = self.work_of(thread)?;
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
                    push_repo(&mut out, "", Some(wt.branch.clone()), diff);
                }
                Work::Copy(wt) => {
                    for r in &wt.repos {
                        let dir = wt.path.join(&r.repo);
                        if !dir.is_dir() {
                            continue;
                        }
                        let copied = copied_in(&project.copy_files, &r.repo);
                        let diff =
                            worktree::diff_scoped(&dir, "HEAD", &copied, None, &prefix(&r.repo))?;
                        push_repo(&mut out, &r.repo, Some(wt.branch.clone()), diff);
                    }
                }
                Work::InPlace => {
                    let found = in_place_changes(&project.root, &project.kind, &edits);
                    for (repo, paths) in found.repos {
                        let dir = project.root.join(&repo);
                        let diff =
                            worktree::diff_scoped(&dir, "HEAD", &[], Some(&paths), &prefix(&repo))?;
                        push_repo(&mut out, &repo, worktree::current_branch(&dir), diff);
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
        let (project, work) = self.work_of(thread)?;
        let edits = match work {
            Work::InPlace => self.thread_edits(thread, &project.root)?,
            _ => Vec::new(),
        };
        let repo = repo.filter(|r| !r.is_empty());
        // Which folder to commit in, and what: everything but the copied
        // files, or only the run's paths.
        let (dir, only, copied) = match &work {
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
        self.claim_for_change(thread, |name| {
            format!("Wait for {name} to finish, or stop it.")
        })?;
        let root = project.root.clone();
        let kind = project.kind.clone();
        let result = blocking(move || match only {
            Some(repo) => {
                let found = in_place_changes(&root, &kind, &edits);
                let paths = found
                    .repos
                    .into_iter()
                    .find(|(r, _)| *r == repo)
                    .map(|(_, p)| p)
                    .unwrap_or_default();
                if paths.is_empty() {
                    return Ok(None);
                }
                worktree::commit_only(&dir, &message, &paths).map(Some)
            }
            None => {
                if worktree::diff(&dir, "HEAD", &copied)?.files.is_empty() {
                    return Ok(None);
                }
                worktree::commit(&dir, &message, &copied).map(Some)
            }
        })
        .await;
        self.after_change(thread, &project);
        if matches!(result, Ok(Some(_))) {
            self.thread_overview_stale(thread);
        }
        match result? {
            Some(sha) => json(sha),
            None => Err("Nothing to commit.".into()),
        }
    }

    /// Answers `Discard` for a folder of repos, or in place. A single
    /// repo's worktree goes through [`Service::discard`].
    pub(crate) async fn discard_in(
        &self,
        thread: ThreadId,
        repos: Option<Vec<String>>,
        delete_branch: bool,
    ) -> Reply {
        let (project, work) = self.work_of(thread)?;
        match work {
            Work::None => Err("This thread has no worktree.".into()),
            Work::Copy(wt) if wt.repos.is_empty() => self.discard(thread, delete_branch).await,
            Work::Copy(wt) => {
                let all: Vec<String> = wt.repos.iter().map(|r| r.repo.clone()).collect();
                let chosen = repos.unwrap_or_else(|| all.clone());
                if let Some(bad) = chosen.iter().find(|r| !all.contains(r)) {
                    return Err(format!("{bad} isn't a repo of this thread."));
                }
                self.claim_for_change(thread, |name| format!("Stop {name} first."))?;
                let (root, own) = (project.root.clone(), self.worktrees().to_path_buf());
                let held = wt.clone();
                let (removed, kept, failed, gone) = blocking(move || {
                    let (removed, kept, failed) =
                        remove_repo_worktrees(&root, &held, &chosen, delete_branch);
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
                self.after_change(thread, &project);
                self.thread_overview_stale(thread);
                if let Some(e) = failed {
                    return Err(e);
                }
                saved?;
                Ok(serde_json::Value::Null)
            }
            Work::InPlace => {
                let all = project.kind.repos();
                let chosen = repos.unwrap_or_else(|| all.clone());
                if let Some(bad) = chosen.iter().find(|r| !all.contains(r)) {
                    return Err(format!("{bad} isn't a repo of this folder."));
                }
                let edits = self.thread_edits(thread, &project.root)?;
                self.claim_for_change(thread, |name| format!("Stop {name} first."))?;
                let (root, kind) = (project.root.clone(), project.kind.clone());
                let result = blocking(move || {
                    let found = in_place_changes(&root, &kind, &edits);
                    let mut left = Vec::new();
                    for (repo, paths) in found.repos {
                        if !chosen.contains(&repo) {
                            continue;
                        }
                        let done = worktree::restore_tracked(&root.join(&repo), &paths)?;
                        left.extend(done.kept.iter().map(|p| format!("{}{p}", prefix(&repo))));
                    }
                    Ok::<_, worktree::WorktreeError>(left)
                })
                .await;
                if let Ok(left) = &result
                    && !left.is_empty()
                {
                    let mut db = self.db();
                    if let Err(e) =
                        self.add_message(&mut db, thread, Author::System, &left_note(left), None)
                    {
                        log(e);
                    }
                }
                self.after_change(thread, &project);
                self.thread_overview_stale(thread);
                result?;
                Ok(serde_json::Value::Null)
            }
        }
    }

    /// Lets runs start in the thread again after a commit or discard, and
    /// drops what may have changed.
    fn after_change(&self, thread: ThreadId, project: &StoredProject) {
        self.changes_may_differ(thread);
        if project.work_mode == WorkMode::InPlace {
            self.project_files_may_differ(project);
        }
        self.release_change(thread);
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
