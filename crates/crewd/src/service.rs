//! Chat state and run handling behind the server.
//!
//! Lock order: `state` before `db`. Neither lock is held across an `.await`,
//! nor while git or a setup command runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crew_core::agent::{AgentSpec, PermissionMode};
use crew_core::claude::{ClaudeInvocation, SessionArg, build_claude};
use crew_core::event::{Event, RunStatus, ToolKind};
use crew_core::ids::{AgentId, MessageId, ProjectId, RunId, ThreadId};
use crew_core::mention::parse_mentions;
use crew_core::scheduler::{Job, Scheduler};
use crew_core::slug::slugify;
use crew_core::store::{Author, RunFilter, Store, StoreError, StoredProject, ThreadWorktree};
use crew_core::worktree::{self, Diff};
use crew_proto::{Request, ServerMsg};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

use crate::runner::{RunHandle, RunOutcome, spawn_run, spawn_setup};

/// Capacity of the notification channel.
const NOTICE_BUFFER: usize = 4096;
/// Name used for the user's messages in a new session's context.
const YOU_NAME: &str = "You";
/// Tool call id of the setup command's events.
const SETUP_ID: &str = "setup";

/// A notification for subscribers of `thread`.
///
/// `serial` grows by one per notification and lets a subscriber skip what
/// its backlog already covered.
#[derive(Debug, Clone)]
pub struct Notice {
    pub serial: u64,
    pub thread: ThreadId,
    pub msg: ServerMsg,
}

/// What a subscriber gets first: past runs and messages of a thread.
pub struct Backlog {
    pub msgs: Vec<ServerMsg>,
    /// Notices with a serial up to this are already in `msgs`.
    pub serial: u64,
}

/// The store plus the serial of the last notice. Notices are sent while
/// this is locked, so a backlog read under the same lock lines up exactly.
struct Db {
    store: Store,
    serial: u64,
}

/// What a queued run needs besides its job.
struct Pending {
    /// The message that asked for the run.
    message: MessageId,
    /// The prompt to use if the run starts a new session.
    fresh: String,
}

struct State {
    scheduler: Scheduler,
    live: HashMap<RunId, (ThreadId, RunHandle)>,
    pending: HashMap<RunId, Pending>,
    /// One task per started run, so shutdown can wait for them.
    watchers: JoinSet<()>,
    /// Set once crewd is shutting down. Nothing starts after that.
    shutting_down: bool,
}

struct Inner {
    db: Mutex<Db>,
    state: Mutex<State>,
    tx: broadcast::Sender<Notice>,
    claude_bin: Option<PathBuf>,
    /// Where thread worktrees go, one folder per project.
    worktrees: PathBuf,
    /// Held while a worktree's slug is picked and the worktree made, so
    /// two threads can't pick the same one.
    slugs: Mutex<()>,
}

/// Holds the store, the scheduler and the live runs.
#[derive(Clone)]
pub struct Service(Arc<Inner>);

type Reply = Result<serde_json::Value, String>;

fn json(v: impl Serialize) -> Reply {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

fn log(msg: impl std::fmt::Display) {
    eprintln!("crewd: {msg}");
}

impl Service {
    /// Wraps an open store. `claude_bin` overrides the binary for every run.
    /// Thread worktrees are made under `worktrees`.
    pub fn new(
        store: Store,
        max_runs: usize,
        claude_bin: Option<PathBuf>,
        worktrees: PathBuf,
    ) -> Service {
        let (tx, _) = broadcast::channel(NOTICE_BUFFER);
        Service(Arc::new(Inner {
            db: Mutex::new(Db { store, serial: 0 }),
            state: Mutex::new(State {
                scheduler: Scheduler::new(max_runs),
                live: HashMap::new(),
                pending: HashMap::new(),
                watchers: JoinSet::new(),
                shutting_down: false,
            }),
            tx,
            claude_bin,
            worktrees,
            slugs: Mutex::new(()),
        }))
    }

    /// A receiver for all notifications from now on.
    pub fn notices(&self) -> broadcast::Receiver<Notice> {
        self.0.tx.subscribe()
    }

    fn db(&self) -> MutexGuard<'_, Db> {
        self.0.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Handles every request except `Hello` and `Subscribe`, which belong
    /// to the connection.
    pub async fn handle(&self, req: Request) -> Reply {
        match req {
            Request::ThreadDiff { thread } => self.thread_diff(thread).await,
            Request::Commit { thread, message } => self.commit(thread, &message).await,
            Request::Discard { thread } => self.discard(thread).await,
            other => self.handle_now(other),
        }
    }

    /// Requests that only touch the store or the scheduler.
    fn handle_now(&self, req: Request) -> Reply {
        match req {
            Request::Hello { .. } | Request::Subscribe { .. } => {
                Err("This request is handled by the connection.".into())
            }
            Request::CreateProject {
                name,
                root,
                main_branch,
            } => json(
                self.db()
                    .store
                    .create_project(&name, Path::new(&root), &main_branch)
                    .map_err(|e| e.to_string())?,
            ),
            Request::CreateChannel { project, name } => json(
                self.db()
                    .store
                    .create_channel(project, &name)
                    .map_err(|e| e.to_string())?,
            ),
            Request::CreateAgent { spec } => json(
                self.db()
                    .store
                    .create_agent(&spec)
                    .map_err(|e| e.to_string())?,
            ),
            Request::CreateThread { channel, title } => json(
                self.db()
                    .store
                    .create_thread(channel, &title)
                    .map_err(|e| e.to_string())?,
            ),
            Request::PostMessage { thread, body } => json(
                self.post_message(thread, &body)
                    .map_err(|e| e.to_string())?,
            ),
            Request::StopRun { run } => {
                self.stop_run(run)?;
                Ok(serde_json::Value::Null)
            }
            Request::StopThread { thread } => {
                self.stop_thread(thread).map_err(|e| e.to_string())?;
                Ok(serde_json::Value::Null)
            }
            Request::StopAll => {
                self.stop_all().map_err(|e| e.to_string())?;
                Ok(serde_json::Value::Null)
            }
            Request::ListRuns { status } => {
                let filter = RunFilter {
                    status,
                    thread: None,
                };
                json(self.db().store.runs(filter).map_err(|e| e.to_string())?)
            }
            Request::ListAgents => json(self.db().store.agents().map_err(|e| e.to_string())?),
            Request::ListProjects => json(self.db().store.projects().map_err(|e| e.to_string())?),
            Request::ListChannels { project } => json(
                self.db()
                    .store
                    .channels(project)
                    .map_err(|e| e.to_string())?,
            ),
            Request::ListThreads { channel } => json(
                self.db()
                    .store
                    .threads(channel)
                    .map_err(|e| e.to_string())?,
            ),
            Request::UpdateProject {
                project,
                setup_command,
                copy_files,
            } => {
                self.db()
                    .store
                    .set_project_setup(project, setup_command.as_deref(), &copy_files)
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::Value::Null)
            }
            Request::ThreadDiff { .. } | Request::Commit { .. } | Request::Discard { .. } => {
                Err("This request needs git and is handled elsewhere.".into())
            }
        }
    }

    /// Uncommitted changes in the thread's worktree, or nothing if it has
    /// none. Files copied in from the project root are left out.
    async fn thread_diff(&self, thread: ThreadId) -> Reply {
        let wt = self
            .db()
            .store
            .thread(thread)
            .map_err(|e| e.to_string())?
            .worktree;
        let Some(wt) = wt else {
            return json(Diff::default());
        };
        let copied = self.copied_files(thread)?;
        json(blocking(move || worktree::diff(&wt.path, "HEAD", &copied)).await?)
    }

    /// The project's `copy_files`, which stay out of diffs and commits.
    fn copied_files(&self, thread: ThreadId) -> Result<Vec<String>, String> {
        let project = self
            .db()
            .store
            .project_of_thread(thread)
            .map_err(|e| e.to_string())?;
        Ok(project.copy_files)
    }

    /// Commits everything in the thread's worktree except files copied in
    /// from the project root. The worktree stays.
    async fn commit(&self, thread: ThreadId, message: &str) -> Reply {
        let message = message.trim().to_string();
        if message.is_empty() {
            return Err("Write a commit message.".into());
        }
        let copied = self.copied_files(thread)?;
        let wt = self.claim_for_change(thread)?;
        let result = blocking(move || {
            if worktree::diff(&wt.path, "HEAD", &copied)?.files.is_empty() {
                return Ok(None);
            }
            worktree::commit(&wt.path, &message, &copied).map(Some)
        })
        .await;
        self.release_change(thread);
        match result? {
            Some(sha) => json(sha),
            None => Err("Nothing to commit.".into()),
        }
    }

    /// Removes the thread's worktree and branch, changes included.
    async fn discard(&self, thread: ThreadId) -> Reply {
        let root = self
            .db()
            .store
            .project_of_thread(thread)
            .map_err(|e| e.to_string())?
            .root;
        let wt = self.claim_for_change(thread)?;
        let path = wt.path.clone();
        let result = blocking(move || worktree::remove(&root, &wt.path, &wt.branch, true)).await;
        // A worktree that is gone is cleared even if deleting the branch failed.
        let cleared = if result.is_ok() || !path.exists() {
            self.db()
                .store
                .set_thread_worktree(thread, None)
                .map_err(|e| e.to_string())
        } else {
            Ok(())
        };
        self.release_change(thread);
        result?;
        cleared?;
        Ok(serde_json::Value::Null)
    }

    /// Holds the thread so no run starts there while its worktree changes.
    /// Refuses while a run in the thread is live.
    fn claim_for_change(&self, thread: ThreadId) -> Result<ThreadWorktree, String> {
        let mut state = self.state();
        let db = self.db();
        let live = state
            .live
            .iter()
            .find(|(_, (t, _))| *t == thread)
            .map(|(run, _)| *run);
        if let Some(run) = live {
            let agents = db.store.agents().map_err(|e| e.to_string())?;
            let agent = db.store.run(run).map_err(|e| e.to_string())?.agent;
            let name = agents
                .iter()
                .find(|(id, _)| *id == agent)
                .map_or("the agent", |(_, spec)| spec.name.as_str());
            return Err(format!("Wait for {name} to finish, or stop it."));
        }
        let wt = db
            .store
            .thread(thread)
            .map_err(|e| e.to_string())?
            .worktree
            .ok_or("This thread has no worktree.")?;
        if !state.scheduler.claim(thread) {
            return Err("Another change to this thread's worktree is in progress.".into());
        }
        Ok(wt)
    }

    /// Lets runs start in the thread again.
    fn release_change(&self, thread: ThreadId) {
        self.state().scheduler.finished(thread);
        self.start_ready();
    }

    /// Past runs (status and events from seq 0) and all messages of a thread.
    pub fn subscribe(&self, thread: ThreadId) -> Result<Backlog, StoreError> {
        let db = self.db();
        db.store.thread_cwd(thread)?;
        let mut msgs = Vec::new();
        let filter = RunFilter {
            status: None,
            thread: Some(thread),
        };
        for run in db.store.runs(filter)? {
            for (seq, event) in db.store.events(run.id, 0)? {
                msgs.push(ServerMsg::Event {
                    thread,
                    run: run.id,
                    seq,
                    event,
                });
            }
            msgs.push(ServerMsg::RunStatus {
                thread,
                run: run.id,
                status: run.status,
            });
        }
        for message in db.store.messages(thread)? {
            msgs.push(ServerMsg::Message { thread, message });
        }
        Ok(Backlog {
            msgs,
            serial: db.serial,
        })
    }

    /// Saves the message, then queues a run for each mentioned agent.
    pub fn post_message(&self, thread: ThreadId, body: &str) -> Result<MessageId, StoreError> {
        let id = {
            let mut state = self.state();
            let mut db = self.db();
            let earlier = db.store.messages(thread)?;
            let agents = db.store.agents()?;
            let id = self.add_message(&mut db, thread, Author::You, body)?;

            let names: Vec<&str> = agents.iter().map(|(_, a)| a.name.as_str()).collect();
            let (known, unknown) = parse_mentions(body, &names);
            let context = render_context(&earlier, &agents);
            let fresh = if context.is_empty() {
                body.to_string()
            } else {
                format!("{context}\n\n{body}")
            };
            for name in known {
                let Some((agent, _)) = agents.iter().find(|(_, a)| a.name == name) else {
                    continue;
                };
                let run = db.store.create_run(thread, *agent)?;
                self.publish_status(&mut db, thread, run, RunStatus::Queued);
                state.scheduler.enqueue(Job {
                    run,
                    thread,
                    agent: *agent,
                    prompt: body.to_string(),
                });
                state.pending.insert(
                    run,
                    Pending {
                        message: id,
                        fresh: fresh.clone(),
                    },
                );
            }
            for name in unknown {
                self.add_message(
                    &mut db,
                    thread,
                    Author::System,
                    &format!("No agent named @{name}."),
                )?;
            }
            id
        };
        self.start_ready();
        Ok(id)
    }

    /// Stops a live run, or takes a queued one out of the queue.
    fn stop_run(&self, run: RunId) -> Result<(), String> {
        let mut state = self.state();
        if let Some((_, handle)) = state.live.get(&run) {
            handle.stop();
            return Ok(());
        }
        match state.scheduler.remove(run) {
            Some(job) => {
                state.pending.remove(&run);
                self.note_dropped(&[job], RunStatus::Stopped)
                    .map_err(|e| e.to_string())
            }
            None => Err(format!("Run {run} isn't queued or running.")),
        }
    }

    /// Stops the thread's live run and drops its queued jobs.
    fn stop_thread(&self, thread: ThreadId) -> Result<(), StoreError> {
        let mut state = self.state();
        for (t, handle) in state.live.values() {
            if *t == thread {
                handle.stop();
            }
        }
        let jobs = state.scheduler.clear();
        let (dropped, kept): (Vec<Job>, Vec<Job>) =
            jobs.into_iter().partition(|j| j.thread == thread);
        for job in kept {
            state.scheduler.enqueue(job);
        }
        for job in &dropped {
            state.pending.remove(&job.run);
        }
        self.note_dropped(&dropped, RunStatus::Stopped)
    }

    /// Stops every live run and drops the whole queue.
    fn stop_all(&self) -> Result<(), StoreError> {
        let mut state = self.state();
        for (_, handle) in state.live.values() {
            handle.stop();
        }
        let dropped = state.scheduler.clear();
        state.pending.clear();
        self.note_dropped(&dropped, RunStatus::Stopped)
    }

    /// Stops every run for good and waits until all of them have ended.
    /// Live runs end as `Interrupted`, and so do queued ones. Runs stop
    /// at the same time, so this takes at most one stop escalation.
    pub async fn shutdown(&self) {
        let mut watchers = {
            let mut state = self.state();
            state.shutting_down = true;
            for (_, handle) in state.live.values() {
                handle.stop();
            }
            let dropped = state.scheduler.clear();
            state.pending.clear();
            if let Err(e) = self.note_dropped(&dropped, RunStatus::Interrupted) {
                log(format!("couldn't record the queued runs: {e}"));
            }
            std::mem::take(&mut state.watchers)
        };
        while watchers.join_next().await.is_some() {}
    }

    /// Records queued runs that will never start, and tells each thread.
    /// Callers hold the state lock.
    fn note_dropped(&self, dropped: &[Job], status: RunStatus) -> Result<(), StoreError> {
        let mut db = self.db();
        let mut threads: Vec<ThreadId> = Vec::new();
        for job in dropped {
            db.store.set_run_status(job.run, status.clone())?;
            self.publish_status(&mut db, job.thread, job.run, status.clone());
            if !threads.contains(&job.thread) {
                threads.push(job.thread);
            }
        }
        if status == RunStatus::Stopped {
            for thread in threads {
                self.add_message(&mut db, thread, Author::System, "Stopped before starting.")?;
            }
        }
        Ok(())
    }

    /// Starts queued jobs while the scheduler allows.
    fn start_ready(&self) {
        let mut state = self.state();
        if state.shutting_down {
            return;
        }
        while let Some(job) = state.scheduler.next_ready() {
            let pending = state.pending.remove(&job.run);
            // Forget watchers that are done, so the set stays small.
            while state.watchers.try_join_next().is_some() {}
            if self.needs_worktree(&job) {
                self.begin_setup(&mut state, job, pending);
            } else if let Some(watch) = self.launch(&mut state, job, pending, true) {
                state.watchers.spawn(watch);
            }
        }
    }

    /// Whether the job's agent may write and the thread has no worktree,
    /// or one whose setup didn't finish.
    /// Store errors say no, so `prepare` reports them.
    fn needs_worktree(&self, job: &Job) -> bool {
        let db = self.db();
        let writes = db.store.agents().is_ok_and(|agents| {
            agents.iter().any(|(id, spec)| {
                *id == job.agent && spec.permission_mode != PermissionMode::ReadOnly
            })
        });
        writes
            && db
                .store
                .thread(job.thread)
                .is_ok_and(|t| t.worktree.is_none_or(|wt| !wt.ready))
    }

    /// Marks the run `Working` and makes the thread's worktree in a task of
    /// its own. The run counts as live, so it can be stopped meanwhile.
    fn begin_setup(&self, state: &mut State, job: Job, pending: Option<Pending>) {
        let (handle, stop_rx) = RunHandle::idle();
        {
            let mut db = self.db();
            match db.store.set_run_status(job.run, RunStatus::Working) {
                Ok(()) => self.publish_status(&mut db, job.thread, job.run, RunStatus::Working),
                Err(e) => log(format!("couldn't mark run {} working: {e}", job.run)),
            }
        }
        state.live.insert(job.run, (job.thread, handle));
        let svc = self.clone();
        state
            .watchers
            .spawn(svc.set_up_then_run(job, pending, stop_rx));
    }

    /// Spawns `claude` for a job. Returns the task that watches it, or
    /// `None` if it couldn't start, in which case the run is marked failed.
    /// `announce` publishes `Working`, which a run that was set up did already.
    fn launch(
        &self,
        state: &mut State,
        job: Job,
        pending: Option<Pending>,
        announce: bool,
    ) -> Option<impl Future<Output = ()> + Send + 'static> {
        let mut db = self.db();
        match self.prepare(&mut db, &job, pending, announce) {
            Ok((name, inv)) => {
                drop(db);
                let (handle, rx, join) = spawn_run(inv);
                state.live.insert(job.run, (job.thread, handle));
                Some(
                    self.clone()
                        .watch(job.thread, job.run, job.agent, name, rx, join),
                )
            }
            Err(e) => {
                log(format!(
                    "couldn't start a run in thread {}: {e}",
                    job.thread
                ));
                state.live.remove(&job.run);
                state.scheduler.finished(job.thread);
                match db.store.set_run_status(job.run, RunStatus::Failed) {
                    Ok(()) => self.publish_status(&mut db, job.thread, job.run, RunStatus::Failed),
                    Err(e) => log(e),
                }
                let body = format!("Couldn't start the run: {e}");
                if let Err(e) = self.add_message(&mut db, job.thread, Author::System, &body) {
                    log(e);
                }
                None
            }
        }
    }

    /// Makes the thread's worktree, then starts `claude` in it unless the
    /// run was stopped. A failed or stopped setup ends the run.
    async fn set_up_then_run(
        self,
        job: Job,
        pending: Option<Pending>,
        stop_rx: watch::Receiver<bool>,
    ) {
        let next = match self.set_up_worktree(&job, stop_rx).await {
            Ok(()) => {
                let mut state = self.state();
                let stopped = state
                    .live
                    .get(&job.run)
                    .is_none_or(|(_, handle)| handle.is_stopped());
                if stopped || state.shutting_down {
                    Err(RunOutcome::Stopped)
                } else {
                    Ok(self.launch(&mut state, job.clone(), pending, false))
                }
            }
            Err(outcome) => Err(outcome),
        };
        match next {
            Ok(Some(watch)) => watch.await,
            Ok(None) => self.start_ready(),
            Err(outcome) => {
                let name = self.agent_name(job.agent);
                if let Err(e) = self.finish(
                    job.thread,
                    job.run,
                    job.agent,
                    &name,
                    outcome,
                    String::new(),
                ) {
                    log(format!("couldn't record the end of run {}: {e}", job.run));
                }
                self.start_ready();
            }
        }
    }

    fn agent_name(&self, agent: AgentId) -> String {
        let agents = self.db().store.agents().unwrap_or_default();
        agents
            .into_iter()
            .find(|(id, _)| *id == agent)
            .map_or_else(|| "The agent".to_string(), |(_, spec)| spec.name)
    }

    /// Makes the worktree, or reuses one whose setup didn't finish, copies
    /// the project's files into it and runs its setup command there. Only
    /// then is the worktree marked ready. The setup command's output
    /// becomes run events.
    async fn set_up_worktree(
        &self,
        job: &Job,
        stop_rx: watch::Receiver<bool>,
    ) -> Result<(), RunOutcome> {
        let failed =
            |what: &str, e: &dyn std::fmt::Display| RunOutcome::Failed(format!("{what}: {e}"));
        let (project, project_slug, thread) = {
            let db = self.db();
            let found = db.store.project_of_thread(job.thread).and_then(|project| {
                let projects = db.store.projects()?;
                let thread = db.store.thread(job.thread)?;
                let slug = project_slug(&projects, project.id);
                Ok((project, slug, thread))
            });
            found.map_err(|e| failed("Couldn't make the worktree", &e))?
        };

        // A retry copies only the files that are missing, so edits stay.
        let retry = thread.worktree.is_some();
        let wt = match thread.worktree {
            Some(wt) => wt,
            None => {
                let dir = self.0.worktrees.join(project_slug);
                let svc = self.clone();
                let (root, main, title) = (
                    project.root.clone(),
                    project.main_branch.clone(),
                    thread.title,
                );
                let wt = blocking(move || {
                    let _held = svc.0.slugs.lock().unwrap_or_else(PoisonError::into_inner);
                    let taken = |slug: &str| {
                        dir.join(slug).exists()
                            || worktree::branch_exists(&root, &format!("crew/{slug}"))
                                .unwrap_or(false)
                    };
                    let slug = slugify(&title, &taken);
                    let path = dir.join(&slug);
                    let branch = format!("crew/{slug}");
                    let base = worktree::create(&root, &main, &path, &branch)?;
                    Ok::<_, worktree::WorktreeError>(ThreadWorktree {
                        path,
                        branch,
                        base,
                        ready: false,
                    })
                })
                .await
                .map_err(|e| failed("Couldn't make the worktree", &e))?;
                self.db()
                    .store
                    .set_thread_worktree(job.thread, Some(&wt))
                    .map_err(|e| failed("Couldn't save the worktree", &e))?;
                wt
            }
        };

        if !wt.path.is_dir() {
            let msg = format!("The worktree folder {} is missing.", wt.path.display());
            return Err(RunOutcome::Failed(msg));
        }
        let (root, dest, files) = (project.root.clone(), wt.path.clone(), project.copy_files);
        let skipped = blocking(move || {
            let files: Vec<String> = if retry {
                files
                    .into_iter()
                    .filter(|f| !dest.join(f).exists())
                    .collect()
            } else {
                files
            };
            worktree::copy_files(&root, &dest, &files)
        })
        .await
        .map_err(|e| failed("Couldn't copy files into the worktree", &e))?;
        if !skipped.is_empty() {
            let body = format!("Skipped missing files: {}.", skipped.join(", "));
            let mut db = self.db();
            if let Err(e) = self.add_message(&mut db, job.thread, Author::System, &body) {
                log(e);
            }
        }

        if let Some(command) = project.setup_command.filter(|c| !c.trim().is_empty()) {
            self.run_setup(job, &command, &wt.path, stop_rx).await?;
        }
        let ready = ThreadWorktree { ready: true, ..wt };
        self.db()
            .store
            .set_thread_worktree(job.thread, Some(&ready))
            .map_err(|e| failed("Couldn't save the worktree", &e))
    }

    /// Runs the project's setup command in the worktree, recording its
    /// output as events of the job's run.
    async fn run_setup(
        &self,
        job: &Job,
        command: &str,
        cwd: &Path,
        stop_rx: watch::Receiver<bool>,
    ) -> Result<(), RunOutcome> {
        if *stop_rx.borrow() || stop_rx.has_changed().is_err() {
            return Err(RunOutcome::Stopped);
        }
        let result = |output: String, is_error: bool| Event::ToolResult {
            id: SETUP_ID.into(),
            is_error,
            output,
        };
        self.record_event(
            job.thread,
            job.run,
            Event::ToolCall {
                id: SETUP_ID.into(),
                name: "Setup".into(),
                kind: ToolKind::Run,
                target: Some(command.to_string()),
                input: serde_json::json!({ "command": command }),
            },
        );
        let (mut lines, join) = spawn_setup(command, cwd, stop_rx);
        while let Some(output) = lines.recv().await {
            self.record_event(job.thread, job.run, result(output, false));
        }
        let outcome = join
            .await
            .unwrap_or_else(|e| RunOutcome::Failed(format!("The setup task ended: {e}")));
        match outcome {
            RunOutcome::Done => Ok(()),
            RunOutcome::Failed(msg) => {
                let first = msg.lines().next().unwrap_or_default().to_string();
                self.record_event(job.thread, job.run, result(first, true));
                Err(RunOutcome::Failed(msg))
            }
            RunOutcome::Stopped => Err(RunOutcome::Stopped),
        }
    }

    /// Picks the session and prompt for a job, marks its run `Working` and
    /// builds the command line.
    fn prepare(
        &self,
        db: &mut Db,
        job: &Job,
        pending: Option<Pending>,
        announce: bool,
    ) -> Result<(String, ClaudeInvocation), StoreError> {
        let agents = db.store.agents()?;
        let spec = agents
            .iter()
            .find(|(id, _)| *id == job.agent)
            .map(|(_, spec)| spec.clone())
            .ok_or(StoreError::NotFound("agent"))?;
        let cwd = db.store.thread_cwd(job.thread)?;
        let filter = RunFilter {
            status: None,
            thread: Some(job.thread),
        };
        // Resume the agent's latest run here that really started a session.
        // A run that failed before `claude` came up has nothing to resume.
        let mut previous = None;
        for run in db.store.runs(filter)?.into_iter().rev() {
            if run.agent != job.agent || run.session_id.is_empty() {
                continue;
            }
            let started = db
                .store
                .events(run.id, 0)?
                .iter()
                .any(|(_, e)| matches!(e, Event::SessionStarted { .. }));
            if started {
                previous = Some(run);
                break;
            }
        }
        let (session_id, session, prompt) = match previous {
            Some(run) => {
                let messages = db.store.messages(job.thread)?;
                let missed = missed_messages(
                    &messages,
                    run.started_at,
                    pending.as_ref().map(|p| p.message),
                    job.agent,
                );
                let context = render_context(&missed, &agents);
                let prompt = if context.is_empty() {
                    job.prompt.clone()
                } else {
                    format!("{context}\n\n{}", job.prompt)
                };
                (
                    run.session_id.clone(),
                    SessionArg::Resume(run.session_id),
                    prompt,
                )
            }
            None => {
                let id = Uuid::new_v4();
                let prompt = pending.map_or_else(|| job.prompt.clone(), |p| p.fresh);
                (id.to_string(), SessionArg::New(id), prompt)
            }
        };
        db.store.start_run(job.run, &session_id)?;
        if announce {
            self.publish_status(db, job.thread, job.run, RunStatus::Working);
        }
        let mut inv = build_claude(&spec, &prompt, &cwd, session);
        if let Some(bin) = &self.0.claude_bin {
            inv.bin = bin.clone();
        }
        Ok((spec.name, inv))
    }

    /// Saves and forwards a run's events, then records how it ended.
    async fn watch(
        self,
        thread: ThreadId,
        run: RunId,
        agent: AgentId,
        name: String,
        mut rx: mpsc::Receiver<Event>,
        join: JoinHandle<RunOutcome>,
    ) {
        let mut summary = String::new();
        let mut texts: Vec<String> = Vec::new();
        while let Some(event) = rx.recv().await {
            match &event {
                Event::TurnEnd { summary: s, .. } => summary = s.clone(),
                Event::Message { text } => texts.push(text.clone()),
                _ => {}
            }
            self.record_event(thread, run, event);
        }
        let outcome = join
            .await
            .unwrap_or_else(|e| RunOutcome::Failed(format!("The run task ended: {e}")));
        let reply = if summary.is_empty() {
            texts.join("\n")
        } else {
            summary
        };
        if let Err(e) = self.finish(thread, run, agent, &name, outcome, reply) {
            log(format!("couldn't record the end of run {run}: {e}"));
        }
        self.start_ready();
    }

    fn finish(
        &self,
        thread: ThreadId,
        run: RunId,
        agent: AgentId,
        name: &str,
        outcome: RunOutcome,
        reply: String,
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        // The process is gone, so dropping the handle does nothing more.
        state.live.remove(&run);
        state.scheduler.finished(thread);
        let mut db = self.db();
        let (status, author, body) = match outcome {
            RunOutcome::Stopped if state.shutting_down => (
                RunStatus::Interrupted,
                Author::System,
                format!("{name} was interrupted because crewd stopped."),
            ),
            RunOutcome::Done => (RunStatus::Done, Author::Agent(agent), reply),
            RunOutcome::Failed(msg) => (
                RunStatus::Failed,
                Author::System,
                format!("{name} failed: {msg}"),
            ),
            RunOutcome::Stopped => (
                RunStatus::Stopped,
                Author::System,
                format!("{name} stopped."),
            ),
        };
        db.store.set_run_status(run, status.clone())?;
        if !body.is_empty() {
            self.add_message(&mut db, thread, author, &body)?;
        }
        self.publish_status(&mut db, thread, run, status);
        Ok(())
    }

    /// Saves a run's event and notifies the thread.
    fn record_event(&self, thread: ThreadId, run: RunId, event: Event) {
        let mut db = self.db();
        match db.store.append_event(run, &event) {
            Ok(seq) => self.publish(
                &mut db,
                thread,
                ServerMsg::Event {
                    thread,
                    run,
                    seq,
                    event,
                },
            ),
            Err(e) => log(format!("couldn't save an event of run {run}: {e}")),
        }
    }

    fn publish_status(&self, db: &mut Db, thread: ThreadId, run: RunId, status: RunStatus) {
        self.publish(
            db,
            thread,
            ServerMsg::RunStatus {
                thread,
                run,
                status,
            },
        );
    }

    /// Saves a message and notifies the thread.
    fn add_message(
        &self,
        db: &mut Db,
        thread: ThreadId,
        author: Author,
        body: &str,
    ) -> Result<MessageId, StoreError> {
        let id = db.store.add_message(thread, author, body, None)?;
        let message = db
            .store
            .messages(thread)?
            .into_iter()
            .rev()
            .find(|m| m.id == id)
            .ok_or(StoreError::NotFound("message"))?;
        self.publish(db, thread, ServerMsg::Message { thread, message });
        Ok(id)
    }

    fn publish(&self, db: &mut Db, thread: ThreadId, msg: ServerMsg) {
        db.serial += 1;
        // No receivers is fine: nobody is listening.
        let _ = self.0.tx.send(Notice {
            serial: db.serial,
            thread,
            msg,
        });
    }
}

/// Runs blocking work, such as git, off the async threads.
async fn blocking<T, E>(f: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, String>
where
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(e) => Err(format!("The task ended: {e}")),
    }
}

/// The folder name of a project's worktrees: its name as a slug, with
/// `-2`, `-3`, ... for projects made later under the same slug.
fn project_slug(projects: &[StoredProject], id: ProjectId) -> String {
    let mut taken: Vec<String> = Vec::new();
    for project in projects {
        let slug = slugify(&project.name, &|s| taken.iter().any(|t| t == s));
        if project.id == id {
            return slug;
        }
        taken.push(slug);
    }
    slugify("project", &|_| false)
}

/// Messages an agent hasn't seen since its last run started: those after
/// `since` (Unix ms) and before the one that asked for this run, minus the
/// agent's own.
fn missed_messages(
    messages: &[crew_core::store::StoredMessage],
    since: i64,
    asked_by: Option<MessageId>,
    agent: AgentId,
) -> Vec<crew_core::store::StoredMessage> {
    messages
        .iter()
        .take_while(|m| Some(m.id) != asked_by)
        .filter(|m| m.created_at > since && m.author != Author::Agent(agent))
        .cloned()
        .collect()
}

/// Messages as plain text, one per line.
fn render_context(
    messages: &[crew_core::store::StoredMessage],
    agents: &[(AgentId, AgentSpec)],
) -> String {
    messages
        .iter()
        .map(|m| {
            let who = match &m.author {
                Author::You => YOU_NAME,
                Author::System => "System",
                Author::Agent(id) => agents
                    .iter()
                    .find(|(a, _)| a == id)
                    .map(|(_, spec)| spec.name.as_str())
                    .unwrap_or("Agent"),
            };
            format!("{who}: {}", m.body)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
