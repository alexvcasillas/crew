//! A project at a glance and an agent's profile.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crew_core::edits::run_edits;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ProjectId, RunId, ThreadId};
use crew_core::store::StoredRun;
use crew_core::worktree;
use crew_proto::{
    AgentProfile, AgentStats, AgentUse, ChannelRow, ProjectOverview, RecentThread, RunRow,
    SetupFile, SetupFileSize, ThreadState,
};

use crate::overview::{WEEK_MS, ran_since, run_ms};
use crate::service::{Reply, Service, blocking, json, now_ms};

/// Threads in a project overview's recent list.
const RECENT_THREADS: usize = 10;
/// Runs in an agent profile's recent list.
const RECENT_RUNS: usize = 20;

/// Where a thread stands, from its runs in order and whether an approval
/// waits for you there.
pub(crate) fn thread_state(runs: &[StoredRun], needs_you: bool) -> ThreadState {
    if needs_you {
        return ThreadState::NeedsYou;
    }
    if let Some(r) = runs.iter().find(|r| r.status == RunStatus::Working) {
        return ThreadState::Working { agent: r.agent };
    }
    if let Some(r) = runs.iter().find(|r| r.status == RunStatus::Queued) {
        return ThreadState::Queued { agent: r.agent };
    }
    match runs.last().map(|r| &r.status) {
        None | Some(RunStatus::Queued | RunStatus::Working) => ThreadState::Idle,
        Some(RunStatus::Done) => ThreadState::Done,
        Some(RunStatus::Failed) => ThreadState::Failed,
        Some(RunStatus::Stopped | RunStatus::Interrupted) => ThreadState::Stopped,
    }
}

/// How big a setup file is in the project folder.
fn setup_size(root: &Path, path: &str) -> SetupFileSize {
    let full = root.join(path);
    match std::fs::metadata(&full) {
        Ok(meta) if meta.is_file() => SetupFileSize::File { bytes: meta.len() },
        Ok(meta) if meta.is_dir() => SetupFileSize::Folder {
            files: count_files(&full),
        },
        _ => SetupFileSize::Missing,
    }
}

/// Files under `dir`, links not followed.
fn count_files(dir: &Path) -> u32 {
    let mut n = 0u32;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => todo.push(entry.path()),
                Ok(t) if t.is_file() => n = n.saturating_add(1),
                _ => {}
            }
        }
    }
    n
}

impl Service {
    /// Answers `ProjectOverview`. Channel stats come from the channel
    /// overviews, so they share their cached change counts.
    pub(crate) async fn project_overview(&self, project: ProjectId) -> Reply {
        let (stored, channels, threads, runs, waiting) = {
            let db = self.db();
            let found = db.store.project(project).and_then(|p| {
                Ok((
                    p,
                    db.store.channels(project)?,
                    db.store.project_threads(project)?,
                    db.store.project_runs(project)?,
                    db.store.pending_approvals(None)?,
                ))
            });
            found.map_err(|e| e.to_string())?
        };
        let waiting: HashSet<ThreadId> = waiting.into_iter().map(|a| a.thread).collect();

        let mut jobs = tokio::task::JoinSet::new();
        for (i, c) in channels.iter().enumerate() {
            let (svc, id) = (self.clone(), c.id);
            jobs.spawn(async move { (i, svc.channel_overview_data(id).await) });
        }
        let mut overviews = vec![None; channels.len()];
        while let Some(done) = jobs.join_next().await {
            let (i, overview) = done.map_err(|e| format!("The overview task ended: {e}"))?;
            overviews[i] = Some(overview?);
        }

        let mut out = ProjectOverview::default();
        let mut activity: HashMap<ThreadId, i64> = HashMap::new();
        for (c, overview) in channels.iter().zip(overviews.into_iter().flatten()) {
            for t in &overview.threads {
                activity.insert(t.thread, t.last_activity_at);
            }
            out.channels.push(ChannelRow {
                channel: c.id,
                counting: overview.threads.iter().any(|t| t.counting),
                stats: overview.stats,
            });
        }

        let mut runs_of: HashMap<ThreadId, Vec<StoredRun>> = HashMap::new();
        for r in &runs {
            runs_of.entry(r.thread).or_default().push(r.clone());
        }
        let mut recent: Vec<_> = threads
            .iter()
            .filter(|t| t.archived_at.is_none())
            .map(|t| {
                let at = activity.get(&t.id).copied().unwrap_or(t.created_at);
                (at, t)
            })
            .collect();
        recent.sort_by_key(|a| std::cmp::Reverse(a.0));
        for (at, t) in recent.into_iter().take(RECENT_THREADS) {
            let own = runs_of.get(&t.id).map_or(&[][..], Vec::as_slice);
            let mut agents: Vec<AgentId> = Vec::new();
            for r in own {
                if !agents.contains(&r.agent) {
                    agents.push(r.agent);
                }
            }
            out.recent_threads.push(RecentThread {
                thread: t.id,
                channel: t.channel,
                agents,
                last_activity_at: at,
                state: thread_state(own, waiting.contains(&t.id)),
            });
        }

        let since = now_ms() - WEEK_MS;
        let mut used: Vec<(AgentId, u32, usize)> = Vec::new();
        for (order, r) in runs.iter().enumerate() {
            let week = u32::from(ran_since(r, since));
            match used.iter_mut().find(|(a, ..)| *a == r.agent) {
                Some(u) => {
                    u.1 += week;
                    u.2 = order;
                }
                None => used.push((r.agent, week, order)),
            }
        }
        used.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
        out.agents = used
            .into_iter()
            .map(|(agent, runs_week, _)| AgentUse { agent, runs_week })
            .collect();

        let worktrees: Vec<_> = threads
            .iter()
            .filter_map(|t| t.worktree.as_ref().map(|w| w.path.clone()))
            .collect();
        let (root, copy_files) = (stored.root.clone(), stored.copy_files.clone());
        let (info, on_disk, setup) = blocking(move || {
            let info = worktree::root_info(&root);
            let on_disk = worktrees.iter().filter(|p| p.is_dir()).count();
            let setup: Vec<SetupFile> = copy_files
                .into_iter()
                .map(|path| SetupFile {
                    size: setup_size(&root, &path),
                    path,
                })
                .collect();
            Ok::<_, String>((info, on_disk, setup))
        })
        .await?;
        out.root = info;
        out.worktrees = u32::try_from(on_disk).unwrap_or(u32::MAX);
        out.setup_files = setup;
        json(out)
    }

    /// Answers `AgentProfile`, off the async threads: it reads the events
    /// of this week's and the recent runs.
    pub(crate) async fn agent_profile(&self, agent: AgentId) -> Reply {
        let svc = self.clone();
        json(blocking(move || svc.agent_profile_now(agent)).await?)
    }

    fn agent_profile_now(&self, agent: AgentId) -> Result<AgentProfile, String> {
        let err = |e: crew_core::store::StoreError| e.to_string();
        let (runs, waiting, handoffs) = {
            let db = self.db();
            db.store.agent(agent).map_err(err)?;
            let waiting: HashSet<RunId> = db
                .store
                .pending_approvals(None)
                .map_err(err)?
                .into_iter()
                .map(|a| a.run)
                .collect();
            (
                db.store.agent_runs(agent).map_err(err)?,
                waiting,
                db.store.handoffs_since(now_ms() - WEEK_MS).map_err(err)?,
            )
        };
        let now = now_ms();
        let since = now - WEEK_MS;
        let started = |r: &StoredRun| r.status == RunStatus::Working || !r.session_id.is_empty();

        let mut stats = AgentStats::default();
        let mut finished_ms = 0i64;
        for r in &runs {
            if ran_since(r, since) {
                stats.runs_week += 1;
            } else if started(r) && r.ended_at.is_some_and(|e| e >= since - WEEK_MS) {
                stats.runs_last_week += 1;
            }
            if r.ended_at.is_some_and(|e| e >= since) && !r.session_id.is_empty() {
                stats.finished_week += 1;
                finished_ms += run_ms(r, now);
                if r.status == RunStatus::Done {
                    stats.clean_week += 1;
                }
            }
        }
        if stats.finished_week > 0 {
            stats.avg_run_ms = finished_ms / i64::from(stats.finished_week);
        }
        stats.handoffs_sent_week = count(handoffs.iter().filter(|(from, _)| *from == agent));
        stats.handoffs_received_week = count(handoffs.iter().filter(|(_, to)| *to == agent));

        let mut recent: Vec<&StoredRun> = runs.iter().filter(|r| started(r)).collect();
        recent.sort_by_key(|r| std::cmp::Reverse(r.started_at));
        recent.truncate(RECENT_RUNS);
        let wanted: HashSet<RunId> = runs
            .iter()
            .filter(|r| ran_since(r, since))
            .chain(recent.iter().copied())
            .map(|r| r.id)
            .collect();
        let mut edits = HashMap::new();
        for run in wanted {
            let events = self.db().store.events(run, 0).map_err(err)?;
            edits.insert(run, run_edits(events.iter().map(|(_, e)| e)));
        }
        for r in runs.iter().filter(|r| ran_since(r, since)) {
            let e = edits.get(&r.id).copied().unwrap_or_default();
            stats.files_week = stats.files_week.saturating_add(e.files);
            stats.added_week = stats.added_week.saturating_add(e.added);
            stats.removed_week = stats.removed_week.saturating_add(e.removed);
        }
        let recent_runs = recent
            .into_iter()
            .map(|r| {
                let e = edits.get(&r.id).copied().unwrap_or_default();
                RunRow {
                    run: r.clone(),
                    ms: run_ms(r, now),
                    files: e.files,
                    added: e.added,
                    removed: e.removed,
                    waiting: waiting.contains(&r.id),
                }
            })
            .collect();

        // Threads it is in, by its latest run there.
        let mut latest: HashMap<ThreadId, i64> = HashMap::new();
        for r in &runs {
            let at = r.ended_at.unwrap_or(r.started_at);
            let entry = latest.entry(r.thread).or_insert(at);
            *entry = (*entry).max(at);
        }
        let mut projects = HashSet::new();
        let mut threads = Vec::new();
        {
            let db = self.db();
            let open: HashSet<ProjectId> = db
                .store
                .projects()
                .map_err(err)?
                .into_iter()
                .map(|p| p.id)
                .collect();
            for (&thread, &at) in &latest {
                let row = db.store.thread(thread).map_err(err)?;
                let project = db.store.channel(row.channel).map_err(err)?.project;
                // Runs in removed projects stay in the history, but the
                // project and its threads are no longer listed.
                if !open.contains(&project) {
                    continue;
                }
                projects.insert(project);
                if row.archived_at.is_none() {
                    threads.push((at, thread));
                }
            }
        }
        threads.sort_by_key(|t| std::cmp::Reverse(t.0));
        Ok(AgentProfile {
            stats,
            projects: count(projects.iter()),
            recent_runs,
            threads: threads.into_iter().map(|(_, t)| t).collect(),
        })
    }
}

fn count<T>(items: impl Iterator<Item = T>) -> u32 {
    u32::try_from(items.count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use crew_core::agent::Runtime;
    use crew_core::event::RunStatus;
    use crew_core::ids::{AgentId, RunId, ThreadId};
    use crew_core::store::StoredRun;
    use crew_proto::ThreadState;

    use crate::profile::thread_state;

    fn run(status: RunStatus, agent: AgentId) -> StoredRun {
        StoredRun {
            id: RunId::new(),
            thread: ThreadId::new(),
            agent,
            session_id: "s".into(),
            status,
            started_at: 1,
            ended_at: None,
            runtime: Runtime::ClaudeCode,
        }
    }

    #[test]
    fn a_thread_state_says_who_works_then_how_it_ended() {
        let (a, b) = (AgentId::new(), AgentId::new());
        assert_eq!(thread_state(&[], false), ThreadState::Idle);
        let done = run(RunStatus::Done, a);
        let working = run(RunStatus::Working, b);
        let queued = run(RunStatus::Queued, a);
        assert_eq!(
            thread_state(&[done.clone(), working.clone(), queued.clone()], false),
            ThreadState::Working { agent: b }
        );
        assert_eq!(
            thread_state(&[done.clone(), queued], false),
            ThreadState::Queued { agent: a }
        );
        assert_eq!(
            thread_state(&[working, done.clone()], true),
            ThreadState::NeedsYou
        );
        assert_eq!(thread_state(&[done], false), ThreadState::Done);
        assert_eq!(
            thread_state(&[run(RunStatus::Interrupted, a)], false),
            ThreadState::Stopped
        );
        assert_eq!(
            thread_state(&[run(RunStatus::Failed, a)], false),
            ThreadState::Failed
        );
    }
}
