//! Chat state and run handling behind the server.
//!
//! Lock order: `state`, then `db`, then `terminal`. No lock is held across
//! an `.await`, nor while git, a setup command or a file read runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::claude::{
    ClaudeInvocation, CrewContext, McpWiring, SessionArg, Teammate, build_claude,
};
use crew_core::event::{Event, RunStatus, ToolKind};
use crew_core::ids::{AgentId, ApprovalId, ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crew_core::mention::parse_mentions;
use crew_core::scheduler::{Job, Scheduler};
use crew_core::slug::slugify;
use crew_core::store::{
    Author, RunFilter, Store, StoreError, StoredApproval, StoredProject, StoredRun, ThreadWorktree,
};
use crew_core::worktree::{self, Diff};
use crew_proto::{PermissionAnswer, Request, ServerMsg};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use uuid::Uuid;

use crate::codex_runner::{CodexInvocation, Hooks, Resume, spawn_codex_run};
use crate::runner::{RunHandle, RunOutcome, spawn_run, spawn_setup};
use crate::terminal::{Cmd, TerminalState};

/// Told once per run when permission prompts can't reach crewd.
pub const NO_CREW_MCP: &str = "Permission prompts are off: crew-mcp wasn't found next to crewd.";
/// Name used for the user's messages in a new session's context.
const YOU_NAME: &str = "You";
/// Tool call id of the setup command's events.
const SETUP_ID: &str = "setup";
/// How long finished runs stay in a `SubscribeAll` snapshot.
const RECENT_RUNS_MS: i64 = 60 * 60 * 1000;

/// A notification. With a `thread`, its subscribers get it, and so do
/// `SubscribeAll` clients unless it is an `Event`. Without one, only
/// `SubscribeAll` clients get it.
///
/// `serial` grows by one per notification and lets a subscriber skip what
/// its backlog already covered.
#[derive(Debug, Clone)]
pub struct Notice {
    pub serial: u64,
    pub thread: Option<ThreadId>,
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
pub(crate) struct Db {
    pub(crate) store: Store,
    serial: u64,
}

impl Db {
    /// The serial of the last notice.
    pub(crate) fn serial(&self) -> u64 {
        self.serial
    }
}

/// What a queued run needs besides its job.
struct Pending {
    /// The message that asked for the run.
    message: MessageId,
    /// The prompt to use if the run starts a new session.
    fresh: String,
}

pub(crate) struct State {
    pub(crate) scheduler: Scheduler,
    pub(crate) live: HashMap<RunId, (ThreadId, RunHandle)>,
    pending: HashMap<RunId, Pending>,
    /// One task per started run, so shutdown can wait for them.
    watchers: JoinSet<()>,
    /// Set once crewd is shutting down. Nothing starts after that.
    shutting_down: bool,
    /// Permission asks waiting for a decision, by approval.
    pub(crate) waiting: HashMap<ApprovalId, oneshot::Sender<PermissionAnswer>>,
    /// Agents each live run handed off to with the tool.
    pub(crate) handed: HashMap<RunId, Vec<AgentId>>,
    /// The time limit of each live run.
    timers: HashMap<RunId, AbortHandle>,
    /// Runs stopped by their time limit, with the limit in minutes.
    timed_out: HashMap<RunId, u32>,
}

struct Inner {
    db: Mutex<Db>,
    state: Mutex<State>,
    tx: broadcast::Sender<Notice>,
    bins: CliBins,
    /// Where thread worktrees go, one folder per project.
    worktrees: PathBuf,
    /// The crew-mcp binary and the crew home it is given, if runs get it.
    crew_mcp: Option<(PathBuf, PathBuf)>,
    /// Held while a worktree's slug is picked and the worktree made, so
    /// two threads can't pick the same one.
    slugs: Mutex<()>,
    /// How long one minute of a run's time limit lasts.
    minute: Duration,
    /// Terminal sessions. Locked after `db` when both are.
    terminal: Mutex<TerminalState>,
    /// Requests to the terminal session watcher, once it runs.
    terminal_cmd: OnceLock<mpsc::UnboundedSender<Cmd>>,
}

/// Holds the store, the scheduler and the live runs.
#[derive(Clone)]
pub struct Service(Arc<Inner>);

pub(crate) type Reply = Result<serde_json::Value, String>;

/// How long `Runtimes` waits for the CLIs before it answers with less,
/// unless set otherwise.
pub const QUICK_ANSWER: Duration = Duration::from_secs(1);

pub(crate) fn json(v: impl Serialize) -> Reply {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

pub(crate) fn log(msg: impl std::fmt::Display) {
    eprintln!("crewd: {msg}");
}

/// Overrides for the agent CLIs crewd runs or looks for. `None` means the
/// default: `CREW_CLAUDE_BIN` or `claude`, and `codex`.
#[derive(Debug, Clone, Default)]
pub struct CliBins {
    /// Used for every Claude Code run.
    pub claude: Option<PathBuf>,
    /// Used for every Codex run.
    pub codex: Option<PathBuf>,
    /// How long `Runtimes` waits for the CLIs before it answers with
    /// less. `None` is [`QUICK_ANSWER`].
    pub runtimes_wait: Option<Duration>,
}

impl Service {
    /// Wraps an open store. `bins` overrides the agent CLIs.
    /// Thread worktrees are made under `worktrees`. `crew_mcp` is the
    /// crew-mcp binary and crew home passed to every run, if any. A run's
    /// time limit counts in units of `minute`. A subscriber may fall up to
    /// `notice_buffer` notifications behind.
    pub fn new(
        store: Store,
        max_runs: usize,
        bins: CliBins,
        worktrees: PathBuf,
        crew_mcp: Option<(PathBuf, PathBuf)>,
        minute: Duration,
        notice_buffer: usize,
    ) -> Service {
        // Nothing waits for permission asks of an earlier crewd any more.
        if let Err(e) = crate::approvals::cancel_stale_permissions(&store) {
            log(format!("couldn't cancel old permission asks: {e}"));
        }
        let (tx, _) = broadcast::channel(notice_buffer.max(1));
        Service(Arc::new(Inner {
            db: Mutex::new(Db { store, serial: 0 }),
            state: Mutex::new(State {
                scheduler: Scheduler::new(max_runs),
                live: HashMap::new(),
                pending: HashMap::new(),
                watchers: JoinSet::new(),
                shutting_down: false,
                waiting: HashMap::new(),
                handed: HashMap::new(),
                timers: HashMap::new(),
                timed_out: HashMap::new(),
            }),
            tx,
            bins,
            worktrees,
            crew_mcp,
            slugs: Mutex::new(()),
            minute,
            terminal: Mutex::new(TerminalState::default()),
            terminal_cmd: OnceLock::new(),
        }))
    }

    /// A receiver for all notifications from now on.
    pub fn notices(&self) -> broadcast::Receiver<Notice> {
        self.0.tx.subscribe()
    }

    pub(crate) fn db(&self) -> MutexGuard<'_, Db> {
        self.0.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The live runs whose process started, by process id. Each process
    /// is its own process group.
    pub(crate) fn live_run_pids(&self) -> Vec<(i32, RunId)> {
        self.state()
            .live
            .iter()
            .filter_map(|(run, (_, handle))| {
                let pid = i32::try_from(handle.pid()?).ok()?;
                Some((pid, *run))
            })
            .collect()
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn terminal(&self) -> MutexGuard<'_, TerminalState> {
        self.0
            .terminal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set_terminal_cmd(&self, tx: mpsc::UnboundedSender<Cmd>) {
        if self.0.terminal_cmd.set(tx).is_err() {
            log("the terminal session watcher was already started");
        }
    }

    pub(crate) fn send_terminal_cmd(&self, cmd: Cmd) -> Result<(), String> {
        let tx = self
            .0
            .terminal_cmd
            .get()
            .ok_or("Terminal sessions are off.")?;
        tx.send(cmd)
            .map_err(|_| "Terminal sessions stopped.".to_string())
    }

    /// Where thread worktrees go.
    pub(crate) fn worktrees(&self) -> &Path {
        &self.0.worktrees
    }

    /// Whether one of Crew's runs uses this session id.
    pub(crate) fn is_crew_session(&self, session: &str) -> bool {
        self.db()
            .store
            .has_run_session(session)
            .unwrap_or_else(|e| {
                log(format!("couldn't look up session {session}: {e}"));
                false
            })
    }

    /// Handles every request except `Hello`, `Subscribe`, `SubscribeAll`
    /// and `PermissionAsk`, which belong to the connection. `ThreadDiff`,
    /// `Commit` and `Discard` run git and `Runtimes` runs the agent CLIs,
    /// so they can take a while.
    pub async fn handle(&self, req: Request) -> Reply {
        match req {
            Request::ThreadDiff { thread } => self.thread_diff(thread).await,
            Request::Commit { thread, message } => self.commit(thread, &message).await,
            Request::Discard { thread } => self.discard(thread).await,
            Request::Runtimes => self.runtimes().await,
            other => self.handle_now(other),
        }
    }

    /// Requests that only touch the store or the scheduler.
    fn handle_now(&self, req: Request) -> Reply {
        match req {
            Request::Hello { .. }
            | Request::Subscribe { .. }
            | Request::SubscribeAll
            | Request::SubscribeTerminal { .. }
            | Request::UnsubscribeTerminal { .. }
            | Request::PermissionAsk { .. } => {
                Err("This request is handled by the connection.".into())
            }
            Request::CreateProject {
                name,
                root,
                main_branch,
            } => {
                let mut db = self.db();
                let project = db
                    .store
                    .create_project(&name, Path::new(&root), &main_branch)
                    .map_err(|e| e.to_string())?;
                self.project_changed(&mut db, project)?;
                json(project)
            }
            Request::CreateChannel { project, name } => {
                let mut db = self.db();
                let channel = db
                    .store
                    .create_channel(project, &name)
                    .map_err(|e| e.to_string())?;
                self.channel_changed(&mut db, channel)?;
                json(channel)
            }
            Request::CreateAgent { spec } => {
                let mut db = self.db();
                let agent = db.store.create_agent(&spec).map_err(|e| e.to_string())?;
                self.agent_changed(&mut db, agent)?;
                json(agent)
            }
            Request::UpdateAgent { agent, spec } => {
                let mut db = self.db();
                db.store
                    .update_agent(agent, &spec)
                    .map_err(|e| e.to_string())?;
                self.agent_changed(&mut db, agent)?;
                Ok(serde_json::Value::Null)
            }
            Request::DeleteAgent { agent } => {
                self.delete_agent(agent)?;
                Ok(serde_json::Value::Null)
            }
            Request::CreateThread { channel, title } => json(
                self.create_thread(channel, &title)
                    .map_err(|e| e.to_string())?,
            ),
            Request::RenameThread { thread, title } => {
                let title = title.trim();
                if title.is_empty() {
                    return Err("Give the thread a title.".into());
                }
                let mut db = self.db();
                db.store
                    .rename_thread(thread, title)
                    .map_err(|e| e.to_string())?;
                self.thread_changed(&mut db, thread)?;
                Ok(serde_json::Value::Null)
            }
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
            Request::ListAgents { include_deleted } => {
                let agents = self.db().store.all_agents().map_err(|e| e.to_string())?;
                json(
                    agents
                        .into_iter()
                        .filter(|a| include_deleted || a.deleted_at.is_none())
                        .collect::<Vec<_>>(),
                )
            }
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
                let mut db = self.db();
                db.store
                    .set_project_setup(project, setup_command.as_deref(), &copy_files)
                    .map_err(|e| e.to_string())?;
                self.project_changed(&mut db, project)?;
                Ok(serde_json::Value::Null)
            }
            Request::ThreadDiff { .. }
            | Request::Commit { .. }
            | Request::Discard { .. }
            | Request::Runtimes => Err("This request is slow and is handled elsewhere.".into()),
            Request::Handoff { run, agent, brief } => json(self.handoff(run, &agent, brief)?),
            Request::ListApprovals { thread } => json(
                self.db()
                    .store
                    .pending_approvals(thread)
                    .map_err(|e| e.to_string())?,
            ),
            Request::Decide {
                approval,
                approve,
                brief,
            } => {
                self.decide(approval, approve, brief)?;
                Ok(serde_json::Value::Null)
            }
            Request::AllowMoreHandoffs { thread, n } => {
                self.allow_more_handoffs(thread, n)
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::Value::Null)
            }
            Request::SetThreadHandoff {
                thread,
                policy,
                budget,
            } => {
                let mut db = self.db();
                db.store
                    .set_thread_handoff(thread, policy, budget)
                    .map_err(|e| e.to_string())?;
                self.thread_changed(&mut db, thread)?;
                Ok(serde_json::Value::Null)
            }
            Request::SetChannelHandoff {
                channel,
                policy,
                budget,
            } => {
                let mut db = self.db();
                db.store
                    .set_channel_handoff(channel, policy, budget)
                    .map_err(|e| e.to_string())?;
                self.channel_changed(&mut db, channel)?;
                Ok(serde_json::Value::Null)
            }
        }
    }

    /// Tells `SubscribeAll` clients the project as it is now.
    fn project_changed(&self, db: &mut Db, project: ProjectId) -> Result<(), String> {
        let project = db.store.project(project).map_err(|e| e.to_string())?;
        self.publish_global(db, ServerMsg::ProjectChanged { project });
        // Sessions may belong to it now.
        self.rescan_terminal();
        Ok(())
    }

    /// The agent CLIs. A check that takes longer than the wait (by
    /// default [`QUICK_ANSWER`]) is
    /// answered with what was found on `PATH`, and `SubscribeAll` clients
    /// get the whole of it once it ends.
    async fn runtimes(&self) -> Reply {
        let (tx, rx) = oneshot::channel();
        let service = self.clone();
        tokio::spawn(async move {
            let full = crate::runtimes::detect(&service.0.bins).await;
            if let Err(full) = tx.send(full) {
                let mut db = service.db();
                service.publish_global(&mut db, ServerMsg::RuntimesChanged { runtimes: full });
            }
        });
        let wait = self.0.bins.runtimes_wait.unwrap_or(QUICK_ANSWER);
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(full)) => json(full),
            _ => json(crate::runtimes::found(&self.0.bins)),
        }
    }

    /// Tells `SubscribeAll` clients the channel as it is now.
    fn channel_changed(&self, db: &mut Db, channel: ChannelId) -> Result<(), String> {
        let channel = db.store.channel(channel).map_err(|e| e.to_string())?;
        self.publish_global(db, ServerMsg::ChannelChanged { channel });
        Ok(())
    }

    /// Tells `SubscribeAll` clients the agent as it is now.
    fn agent_changed(&self, db: &mut Db, agent: AgentId) -> Result<(), String> {
        let agent = db.store.agent(agent).map_err(|e| e.to_string())?;
        self.publish_global(db, ServerMsg::AgentChanged { agent });
        Ok(())
    }

    /// Tells `SubscribeAll` clients the thread as it is now.
    fn thread_changed(&self, db: &mut Db, thread: ThreadId) -> Result<(), String> {
        let thread = db.store.thread(thread).map_err(|e| e.to_string())?;
        self.publish_global(db, ServerMsg::ThreadChanged { thread });
        Ok(())
    }

    /// Saves a thread and tells `SubscribeAll` clients.
    fn create_thread(&self, channel: ChannelId, title: &str) -> Result<ThreadId, StoreError> {
        let mut db = self.db();
        let id = db.store.create_thread(channel, title)?;
        let thread = db.store.thread(id)?;
        self.publish_global(&mut db, ServerMsg::ThreadCreated { channel, thread });
        Ok(id)
    }

    /// Deletes an agent unless it has queued or working runs.
    fn delete_agent(&self, agent: AgentId) -> Result<(), String> {
        // The state lock keeps a run from being queued meanwhile.
        let _state = self.state();
        let mut db = self.db();
        let stored = db.store.agent(agent).map_err(|e| e.to_string())?;
        let busy = db
            .store
            .runs(RunFilter::default())
            .map_err(|e| e.to_string())?
            .iter()
            .any(|r| {
                r.agent == agent && matches!(r.status, RunStatus::Queued | RunStatus::Working)
            });
        if busy {
            return Err(format!("Stop {}'s runs first.", stored.spec.name));
        }
        db.store.delete_agent(agent).map_err(|e| e.to_string())?;
        self.agent_changed(&mut db, agent)?;
        Ok(())
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

    /// Removes the thread's worktree, uncommitted changes included. Its
    /// branch goes too, unless it has commits beyond the worktree's base:
    /// then it is kept and the thread is told.
    async fn discard(&self, thread: ThreadId) -> Reply {
        let root = self
            .db()
            .store
            .project_of_thread(thread)
            .map_err(|e| e.to_string())?
            .root;
        let wt = self.claim_for_change(thread)?;
        let (path, branch) = (wt.path.clone(), wt.branch.clone());
        let result = blocking(move || {
            let keep = worktree::has_commits_beyond(&root, &wt.branch, &wt.base)?;
            worktree::remove(&root, &wt.path, &wt.branch, !keep)?;
            Ok::<_, worktree::WorktreeError>(keep)
        })
        .await;
        // A worktree that is gone is cleared even if deleting the branch failed.
        let cleared = if result.is_ok() || !path.exists() {
            let mut db = self.db();
            let mut cleared = db.store.set_thread_worktree(thread, None);
            if cleared.is_ok() {
                self.thread_changed_or_log(&mut db, thread);
            }
            if cleared.is_ok() && result == Ok(true) {
                let body =
                    format!("Kept branch {branch} because it has commits that aren't merged.");
                cleared = self
                    .add_message(&mut db, thread, Author::System, &body, None)
                    .map(|_| ());
            }
            cleared.map_err(|e| e.to_string())
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
            let agents = db.store.agents_with_deleted().map_err(|e| e.to_string())?;
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

    /// Past runs (status and events from seq 0), all messages and all
    /// approvals of a thread.
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
                status: run.status.clone(),
                details: run,
            });
        }
        // Each approval comes just before the first message about it, as
        // it does live. Approvals no message is about come last.
        let mut approvals: Vec<Option<StoredApproval>> =
            db.store.approvals(thread)?.into_iter().map(Some).collect();
        for message in db.store.messages(thread)? {
            if let Some(id) = message.approval {
                let found = approvals
                    .iter_mut()
                    .find(|a| a.as_ref().is_some_and(|a| a.id == id));
                if let Some(approval) = found.and_then(Option::take) {
                    msgs.push(ServerMsg::Approval { thread, approval });
                }
            }
            msgs.push(ServerMsg::Message { thread, message });
        }
        for approval in approvals.into_iter().flatten() {
            msgs.push(ServerMsg::Approval { thread, approval });
        }
        Ok(Backlog {
            msgs,
            serial: db.serial,
        })
    }

    /// Everything a `SubscribeAll` client starts from, and the serial it
    /// covers.
    pub fn snapshot(&self) -> Result<(ServerMsg, u64), StoreError> {
        let db = self.db();
        let projects = db.store.projects()?;
        let mut channels = Vec::new();
        for project in &projects {
            channels.extend(db.store.channels(project.id)?);
        }
        let mut threads = Vec::new();
        for channel in &channels {
            threads.extend(db.store.threads(channel.id)?);
        }
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
            - RECENT_RUNS_MS;
        let approvals = db.store.pending_approvals(None)?;
        let runs = with_asked_runs(db.store.recent_runs(since)?, &approvals, |r| {
            db.store.run(r)
        })?;
        let msg = ServerMsg::Snapshot {
            projects,
            channels,
            threads,
            agents: db.store.all_agents()?,
            runs,
            approvals,
            terminal_sessions: self.terminal().sessions(),
        };
        Ok((msg, db.serial))
    }

    /// Saves the message, then queues a run for each mentioned agent.
    pub fn post_message(&self, thread: ThreadId, body: &str) -> Result<MessageId, StoreError> {
        let id = {
            let mut state = self.state();
            let mut db = self.db();
            let earlier = db.store.messages(thread)?;
            let agents = db.store.agents()?;
            let id = self.add_message(&mut db, thread, Author::You, body, None)?;

            let names: Vec<&str> = agents.iter().map(|(_, a)| a.name.as_str()).collect();
            let (known, unknown) = parse_mentions(body, &names);
            let context = render_context(&earlier, &db.store.agents_with_deleted()?);
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
                    None,
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
        if state.live.contains_key(&run) {
            self.stop_live(&mut state, run);
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
        let live: Vec<RunId> = state
            .live
            .iter()
            .filter(|(_, (t, _))| *t == thread)
            .map(|(run, _)| *run)
            .collect();
        for run in live {
            self.stop_live(&mut state, run);
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
        let live: Vec<RunId> = state.live.keys().copied().collect();
        for run in live {
            self.stop_live(&mut state, run);
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
            let live: Vec<RunId> = state.live.keys().copied().collect();
            for run in live {
                self.stop_live(&mut state, run);
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

    /// Stops a live run and denies its pending permission asks at once,
    /// so nobody decides them while the run winds down.
    fn stop_live(&self, state: &mut State, run: RunId) {
        if let Some((_, handle)) = state.live.get(&run) {
            handle.stop();
        }
        let mut db = self.db();
        self.cancel_permissions(state, &mut db, run);
    }

    /// Starts the run's time limit, which counts from now: the moment its
    /// `claude` process starts, after any worktree setup.
    fn start_timer(&self, state: &mut State, job: &Job) {
        let minutes = self
            .db()
            .store
            .agents()
            .ok()
            .and_then(|agents| agents.into_iter().find(|(id, _)| *id == job.agent))
            .map_or(crew_core::agent::DEFAULT_MAX_RUN_MINUTES, |(_, spec)| {
                spec.max_run_minutes
            });
        let limit = self.0.minute.saturating_mul(minutes);
        let (svc, run) = (self.clone(), job.run);
        let timer = tokio::spawn(async move {
            tokio::time::sleep(limit).await;
            let mut state = svc.state();
            // A run you already stopped ends as stopped by you.
            if state
                .live
                .get(&run)
                .is_some_and(|(_, handle)| !handle.is_stopped())
            {
                state.timed_out.insert(run, minutes);
                svc.stop_live(&mut state, run);
            }
        });
        state.timers.insert(job.run, timer.abort_handle());
    }

    /// Forgets a run that is no longer live, and its time limit.
    fn end_live(state: &mut State, run: RunId) {
        state.live.remove(&run);
        if let Some(timer) = state.timers.remove(&run) {
            timer.abort();
        }
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
                self.add_message(
                    &mut db,
                    thread,
                    Author::System,
                    "Stopped before starting.",
                    None,
                )?;
            }
        }
        Ok(())
    }

    /// Starts queued jobs while the scheduler allows.
    pub(crate) fn start_ready(&self) {
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

    /// Puts a Codex run's approval requests to you, like a Claude run's
    /// permission asks, and lets it post notes in its thread.
    fn codex_hooks(&self, thread: ThreadId, run: RunId) -> Hooks {
        let (asks, notes) = (self.clone(), self.clone());
        Hooks {
            ask: Box::new(move |tool, input, item| {
                asks.ask_permission(run, tool, input, item)
                    .map(|wait| wait.answer)
            }),
            note: Box::new(move |body| {
                let mut db = notes.db();
                if let Err(e) = notes.add_message(&mut db, thread, Author::System, body, Some(run))
                {
                    log(format!("couldn't post a note of run {run}: {e}"));
                }
            }),
        }
    }

    /// Spawns the agent's CLI for a job. Returns the task that watches it, or
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
                let (handle, rx, join) = match inv {
                    Invocation::Claude(inv) => spawn_run(inv),
                    Invocation::Codex(inv) => {
                        spawn_codex_run(inv, self.codex_hooks(job.thread, job.run))
                    }
                };
                state.live.insert(job.run, (job.thread, handle));
                self.start_timer(state, &job);
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
                Self::end_live(state, job.run);
                state.scheduler.finished(job.thread);
                match db.store.set_run_status(job.run, RunStatus::Failed) {
                    Ok(()) => self.publish_status(&mut db, job.thread, job.run, RunStatus::Failed),
                    Err(e) => log(e),
                }
                let body = format!("Couldn't start the run: {e}");
                let run = Some(job.run);
                if let Err(e) = self.add_message(&mut db, job.thread, Author::System, &body, run) {
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
        let agents = self.db().store.agents_with_deleted().unwrap_or_default();
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
                self.save_worktree(job.thread, &wt)
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
            let run = Some(job.run);
            if let Err(e) = self.add_message(&mut db, job.thread, Author::System, &body, run) {
                log(e);
            }
        }

        if let Some(command) = project.setup_command.filter(|c| !c.trim().is_empty()) {
            self.run_setup(job, &command, &wt.path, stop_rx).await?;
        }
        let ready = ThreadWorktree { ready: true, ..wt };
        self.save_worktree(job.thread, &ready)
            .map_err(|e| failed("Couldn't save the worktree", &e))
    }

    /// Saves the thread's worktree and tells `SubscribeAll` clients.
    fn save_worktree(&self, thread: ThreadId, wt: &ThreadWorktree) -> Result<(), StoreError> {
        let mut db = self.db();
        db.store.set_thread_worktree(thread, Some(wt))?;
        self.thread_changed_or_log(&mut db, thread);
        Ok(())
    }

    /// Like `thread_changed`, logging instead of failing: the change it
    /// reports is already saved.
    pub(crate) fn thread_changed_or_log(&self, db: &mut Db, thread: ThreadId) {
        if let Err(e) = self.thread_changed(db, thread) {
            log(format!("couldn't tell clients about thread {thread}: {e}"));
        }
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
    /// builds what starts the agent's CLI.
    fn prepare(
        &self,
        db: &mut Db,
        job: &Job,
        pending: Option<Pending>,
        announce: bool,
    ) -> Result<(String, Invocation), StoreError> {
        let agents = db.store.agents_with_deleted()?;
        let spec = db.store.agent(job.agent)?;
        if spec.deleted_at.is_some() {
            return Err(StoreError::NotFound("agent"));
        }
        let spec = spec.spec;
        let cwd = db.store.thread_cwd(job.thread)?;
        let filter = RunFilter {
            status: None,
            thread: Some(job.thread),
        };
        // Resume the agent's latest run here that really started a session
        // with the CLI it uses now. A run that failed before its CLI came up
        // has nothing to resume, and one CLI can't resume another's session.
        let mut previous = None;
        for run in db.store.runs(filter)?.into_iter().rev() {
            if run.agent != job.agent || run.session_id.is_empty() || run.runtime != spec.runtime {
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
        // The prompt for a new session: the chat so far, then the prompt.
        let fresh = match &pending {
            Some(p) => p.fresh.clone(),
            // A handoff: the whole thread so far, then the brief.
            None => with_context(&db.store.messages(job.thread)?, &agents, &job.prompt),
        };
        // A resumed session gets what it missed, then the prompt.
        let resume = match previous {
            Some(run) => {
                let messages = db.store.messages(job.thread)?;
                let missed = missed_messages(
                    &messages,
                    run.started_at,
                    pending.as_ref().map(|p| p.message),
                    job.agent,
                );
                Some((run.session_id, with_context(&missed, &agents, &job.prompt)))
            }
            None => None,
        };
        // Crew names a Claude session. Codex names its threads, so a Codex
        // run gets its session once the thread has started or resumed.
        let new_session = Uuid::new_v4();
        let session_id = match (&resume, &spec.runtime) {
            (Some((id, _)), Runtime::ClaudeCode) => id.clone(),
            (None, Runtime::ClaudeCode) => new_session.to_string(),
            (_, Runtime::Codex) => String::new(),
        };
        db.store.start_run(job.run, &spec.runtime, &session_id)?;
        if announce {
            self.publish_status(db, job.thread, job.run, RunStatus::Working);
        }
        // Codex asks through its app-server, so it needs no crew-mcp for that.
        if self.0.crew_mcp.is_none()
            && spec.permission_mode == PermissionMode::EditFiles
            && spec.runtime == Runtime::ClaudeCode
        {
            self.add_message(db, job.thread, Author::System, NO_CREW_MCP, Some(job.run))?;
        }
        let wiring = self.0.crew_mcp.as_ref().map(|(bin, home)| McpWiring {
            crew_mcp_bin: bin.clone(),
            run: job.run,
            crew_home: home.clone(),
        });
        let context = CrewContext {
            project: db.store.project_of_thread(job.thread)?.name,
            team: db
                .store
                .agents()?
                .into_iter()
                .filter(|(id, _)| *id != job.agent)
                .map(|(_, a)| Teammate {
                    edits: a.permission_mode == PermissionMode::EditFiles,
                    name: a.name,
                    role: a.role,
                })
                .collect(),
        };
        let inv = match spec.runtime {
            Runtime::ClaudeCode => {
                let (session, prompt) = match resume {
                    Some((id, prompt)) => (SessionArg::Resume(id), prompt),
                    None => (SessionArg::New(new_session), fresh),
                };
                let mut inv = build_claude(&spec, &prompt, &cwd, session, wiring, Some(&context));
                if let Some(bin) = &self.0.bins.claude {
                    inv.bin = bin.clone();
                }
                Invocation::Claude(inv)
            }
            Runtime::Codex => {
                let developer_instructions = [
                    context.render_codex(&spec, wiring.is_some()),
                    spec.instructions.clone(),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
                Invocation::Codex(CodexInvocation {
                    bin: crate::runtimes::codex_bin(&self.0.bins),
                    agent: spec.name.clone(),
                    cwd,
                    model: spec.model.clone(),
                    permission_mode: spec.permission_mode.clone(),
                    developer_instructions,
                    prompt: fresh,
                    resume: resume.map(|(thread, prompt)| Resume { thread, prompt }),
                    mcp: wiring,
                })
            }
        };
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
                Event::SessionStarted { session_id, .. } => self.name_session(run, session_id),
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

    /// Saves the session a run started if it had none yet, as a Codex run
    /// hasn't: Codex names its threads, and a resume may start a new one.
    /// A later run resumes it.
    fn name_session(&self, run: RunId, session_id: &str) {
        let db = self.db();
        let unnamed = db.store.run(run).is_ok_and(|r| r.session_id.is_empty());
        if unnamed
            && !session_id.is_empty()
            && let Err(e) = db.store.set_run_session(run, session_id)
        {
            log(format!("couldn't save the session of run {run}: {e}"));
        }
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
        Self::end_live(&mut state, run);
        let limit = state.timed_out.remove(&run);
        let handed = state.handed.remove(&run).unwrap_or_default();
        state.scheduler.finished(thread);
        let mut db = self.db();
        self.cancel_permissions(&mut state, &mut db, run);
        let done = matches!(outcome, RunOutcome::Done);
        let (status, author, body) = match outcome {
            RunOutcome::Stopped if state.shutting_down => (
                RunStatus::Interrupted,
                Author::System,
                format!("{name} was interrupted because crewd stopped."),
            ),
            RunOutcome::Stopped if limit.is_some() => (
                RunStatus::Stopped,
                Author::System,
                format!(
                    "{name} hit its {} min limit and was stopped. Its changes are kept.",
                    limit.unwrap_or_default()
                ),
            ),
            RunOutcome::Done => (RunStatus::Done, Author::Agent(agent), reply.clone()),
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
            self.add_message(&mut db, thread, author, &body, Some(run))?;
        }
        self.publish_status(&mut db, thread, run, status);
        if done && !state.shutting_down {
            self.reply_handoffs(&mut state, &mut db, run, agent, &reply, &handed)?;
        }
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

    /// Tells the thread's subscribers that `run` is now `status`. The run
    /// must be saved with that status first.
    pub(crate) fn publish_status(
        &self,
        db: &mut Db,
        thread: ThreadId,
        run: RunId,
        status: RunStatus,
    ) {
        let details = match db.store.run(run) {
            Ok(details) => details,
            Err(e) => {
                log(format!("couldn't read run {run}: {e}"));
                return;
            }
        };
        self.publish(
            db,
            thread,
            ServerMsg::RunStatus {
                thread,
                run,
                status,
                details,
            },
        );
    }

    /// Saves a message and notifies the thread. `run` is the run it is
    /// about, if any.
    pub(crate) fn add_message(
        &self,
        db: &mut Db,
        thread: ThreadId,
        author: Author,
        body: &str,
        run: Option<RunId>,
    ) -> Result<MessageId, StoreError> {
        let id = db.store.add_message(thread, author, body, run)?;
        self.publish_message(db, thread, id)
    }

    /// Saves a system note about `approval`, asked by `run`, and notifies
    /// the thread.
    pub(crate) fn add_approval_message(
        &self,
        db: &mut Db,
        thread: ThreadId,
        body: &str,
        run: RunId,
        approval: ApprovalId,
    ) -> Result<MessageId, StoreError> {
        let id = db.store.add_approval_message(thread, body, run, approval)?;
        self.publish_message(db, thread, id)
    }

    fn publish_message(
        &self,
        db: &mut Db,
        thread: ThreadId,
        id: MessageId,
    ) -> Result<MessageId, StoreError> {
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

    pub(crate) fn publish(&self, db: &mut Db, thread: ThreadId, msg: ServerMsg) {
        self.send_notice(db, Some(thread), msg);
    }

    /// Notifies `SubscribeAll` clients only.
    pub(crate) fn publish_global(&self, db: &mut Db, msg: ServerMsg) {
        self.send_notice(db, None, msg);
    }

    fn send_notice(&self, db: &mut Db, thread: Option<ThreadId>, msg: ServerMsg) {
        db.serial += 1;
        // No receivers is fine: nobody is listening.
        let _ = self.0.tx.send(Notice {
            serial: db.serial,
            thread,
            msg,
        });
    }
}

/// What starts a run: the agent's CLI and how it is called.
enum Invocation {
    Claude(ClaudeInvocation),
    Codex(CodexInvocation),
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

/// `messages` as context, then `prompt`. A prompt that is the last
/// message from you or an agent, as a reply handed on is, isn't repeated.
fn with_context(
    messages: &[crew_core::store::StoredMessage],
    agents: &[(AgentId, AgentSpec)],
    prompt: &str,
) -> String {
    let context = render_context(messages, agents);
    let repeated = messages
        .iter()
        .rev()
        .find(|m| m.author != Author::System)
        .is_some_and(|m| m.body == prompt);
    if context.is_empty() {
        prompt.to_string()
    } else if repeated {
        context
    } else {
        format!("{context}\n\n{prompt}")
    }
}

/// Messages as plain text, one per line.
fn render_context(
    messages: &[crew_core::store::StoredMessage],
    agents: &[(AgentId, AgentSpec)],
) -> String {
    messages
        .iter()
        // A note for you, not for the agents.
        .filter(|m| !(m.author == Author::System && m.body == NO_CREW_MCP))
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

/// `runs` plus the run each approval points to, when it isn't among them
/// already, so every pending approval can name its run and agent.
fn with_asked_runs(
    mut runs: Vec<StoredRun>,
    approvals: &[StoredApproval],
    run: impl Fn(RunId) -> Result<StoredRun, StoreError>,
) -> Result<Vec<StoredRun>, StoreError> {
    for approval in approvals {
        if !runs.iter().any(|r| r.id == approval.run) {
            runs.push(run(approval.run)?);
        }
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::with_asked_runs;
    use crew_core::event::RunStatus;
    use crew_core::ids::{AgentId, ApprovalId, RunId, ThreadId};
    use crew_core::store::{ApprovalKind, ApprovalStatus, StoreError, StoredApproval, StoredRun};

    fn run(status: RunStatus) -> StoredRun {
        StoredRun {
            id: RunId::new(),
            thread: ThreadId::new(),
            agent: AgentId::new(),
            session_id: String::new(),
            status,
            started_at: 1,
            ended_at: None,
            runtime: crew_core::agent::Runtime::ClaudeCode,
        }
    }

    fn ask(run: &StoredRun) -> StoredApproval {
        StoredApproval {
            id: ApprovalId::new(),
            thread: run.thread,
            run: run.id,
            kind: ApprovalKind::Permission {
                tool: "Bash".into(),
                input: serde_json::json!({}),
                tool_use_id: None,
            },
            status: ApprovalStatus::Pending,
            created_at: 1,
            decided_at: None,
        }
    }

    #[test]
    fn the_snapshot_brings_the_runs_approvals_point_to() {
        let recent = run(RunStatus::Working);
        let mut old = run(RunStatus::Done);
        old.ended_at = Some(2);
        let approvals = [ask(&recent), ask(&old), ask(&old)];
        let store = [old.clone()];
        let runs = with_asked_runs(vec![recent.clone()], &approvals, |id| {
            store
                .iter()
                .find(|r| r.id == id)
                .cloned()
                .ok_or(StoreError::NotFound("run"))
        })
        .unwrap();
        assert_eq!(runs, [recent, old], "each run once");
    }
}
