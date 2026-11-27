//! Chat state and run handling behind the server.
//!
//! Lock order: `state`, then `db`, then `terminal`. The change count cache
//! and the file index are each locked on their own, never while taking
//! another lock. No lock is held across an `.await`, nor while git, a
//! setup command or a file read runs.

use std::collections::{HashMap, HashSet};
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
use crew_core::project_kind::{NEEDS_REPO, ProjectKind, WorkMode, detect_kind};
use crew_core::scheduler::{Job, Scheduler};
use crew_core::slug::slugify;
use crew_core::store::{
    ApprovalStatus, Author, RunFilter, Store, StoreError, StoredApproval, StoredProject, StoredRun,
    ThreadWorktree, TitleMode, TitleModel, TitleSettings,
};
use crew_core::worktree;
use crew_proto::{
    Limits, PermissionAnswer, Request, ServerMsg, SkillInfo, SkillRef, SkillSource, skill_key,
};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use uuid::Uuid;

use crate::codex_runner::{CodexInvocation, Hooks, Resume, spawn_codex_run};
use crate::overview::{ChangeCache, LiveEdit};
use crate::runner::{RunHandle, RunOutcome, spawn_run, spawn_setup};
use crate::search::FileIndex;
use crate::terminal::{Cmd, TerminalState};

/// Told once per run when permission prompts can't reach crewd.
pub const NO_CREW_MCP: &str = "Permission prompts are off: crew-mcp wasn't found next to crewd.";
/// Name used for the user's messages in a new session's context.
const YOU_NAME: &str = "You";
/// The note on an approval closed because its project was removed.
const PROJECT_GONE: &str = "Closed because the project was removed.";
/// Tool call id of the setup command's events.
const SETUP_ID: &str = "setup";
/// Longest channel description, in characters.
const MAX_DESCRIPTION: usize = 280;
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

/// Skills found in a project. `codex` is `None` when its listing failed
/// and is to be tried again.
#[derive(Clone)]
struct SkillCache {
    claude: Vec<crate::skills::Found>,
    codex: Option<Vec<crate::skills::Found>>,
}

/// What a queued run needs besides its job.
pub(crate) struct Pending {
    /// The skill the run is for, and the model to use for this run alone.
    pub(crate) skill: Option<SkillRef>,
    pub(crate) model_override: Option<String>,
    /// The message that asked for the run.
    pub(crate) message: MessageId,
    /// The prompt to use if the run starts a new session.
    pub(crate) fresh: String,
}

pub(crate) struct State {
    pub(crate) scheduler: Scheduler,
    pub(crate) live: HashMap<RunId, (ThreadId, RunHandle)>,
    pub(crate) pending: HashMap<RunId, Pending>,
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
    /// Allow rules a queued run gets for itself only: the call you let a
    /// retry make.
    pub(crate) retry_rules: HashMap<RunId, Vec<String>>,
    /// The run writing in each project folder agents work in in place.
    pub(crate) writers: HashMap<ProjectId, RunId>,
    /// Project folders with a commit or discard in place under way.
    pub(crate) changing: std::collections::HashSet<ProjectId>,
}

struct Inner {
    db: Mutex<Db>,
    /// A read-only connection for search, so it never holds `db`. `None`
    /// for an in-memory store, where search reads through `db`.
    reader: Option<Mutex<Store>>,
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
    /// Held while a project is checked against the others and made, so
    /// two can't take one folder at once.
    adding_project: Mutex<()>,
    /// How long one minute of a run's time limit lasts.
    minute: Duration,
    /// Terminal sessions. Locked after `db` when both are.
    terminal: Mutex<TerminalState>,
    /// Requests to the terminal session watcher, once it runs.
    terminal_cmd: OnceLock<mpsc::UnboundedSender<Cmd>>,
    /// Change counts of thread worktrees, for channel overviews.
    changes: Mutex<ChangeCache>,
    /// Woken each time counts were worked out.
    counted: tokio::sync::Notify,
    /// Slots for working out counts, so git runs a few at a time.
    count_slots: Arc<tokio::sync::Semaphore>,
    /// Extra time each count takes, in ms. Tests set it.
    count_delay_ms: std::sync::atomic::AtomicU64,
    /// File lists of project roots and worktrees, for search.
    files: Mutex<FileIndex>,
    /// Claude Code's home, where its skills and plugins are looked for.
    claude_home: OnceLock<PathBuf>,
    /// Skills found per project, until a reload.
    skills: Mutex<HashMap<ProjectId, SkillCache>>,
    /// The skills a Claude Code run's init line listed, by project.
    claude_listed: Mutex<HashMap<ProjectId, std::collections::HashSet<String>>>,
    /// Title runs under way. Shutdown drops them.
    titles: Mutex<JoinSet<()>>,
    /// The time scheduled jobs go by. Tests set it.
    clock: crate::jobs::Clock,
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
    /// For tests only: never generate thread titles.
    pub no_title_runs: bool,
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
        // Nothing is generating the titles an earlier crewd started.
        if let Err(e) = store.end_stale_titles() {
            log(format!("couldn't end old title runs: {e}"));
        }
        // The run cap from the command line or environment only seeds the
        // setting. Once it is saved, the saved value wins.
        if let Err(e) = store.seed_max_runs(u32::try_from(max_runs).unwrap_or(u32::MAX)) {
            log(format!("couldn't save the run cap: {e}"));
        }
        let limits = store.limits().unwrap_or_else(|e| {
            log(format!("couldn't read the limits: {e}"));
            Limits::default()
        });
        let mut scheduler = Scheduler::new(limits.max_runs as usize);
        scheduler.set_paused(limits.paused);
        let (tx, _) = broadcast::channel(notice_buffer.max(1));
        let reader = store.open_reader().unwrap_or_else(|e| {
            log(format!("couldn't open a reader for search: {e}"));
            None
        });
        Service(Arc::new(Inner {
            reader: reader.map(Mutex::new),
            db: Mutex::new(Db { store, serial: 0 }),
            state: Mutex::new(State {
                scheduler,
                live: HashMap::new(),
                pending: HashMap::new(),
                watchers: JoinSet::new(),
                shutting_down: false,
                waiting: HashMap::new(),
                handed: HashMap::new(),
                timers: HashMap::new(),
                timed_out: HashMap::new(),
                retry_rules: HashMap::new(),
                writers: HashMap::new(),
                changing: std::collections::HashSet::new(),
            }),
            tx,
            bins,
            worktrees,
            crew_mcp,
            slugs: Mutex::new(()),
            adding_project: Mutex::new(()),
            minute,
            terminal: Mutex::new(TerminalState::default()),
            terminal_cmd: OnceLock::new(),
            changes: Mutex::new(ChangeCache::default()),
            counted: tokio::sync::Notify::new(),
            count_slots: Arc::new(tokio::sync::Semaphore::new(crate::overview::GIT_AT_ONCE)),
            count_delay_ms: std::sync::atomic::AtomicU64::new(0),
            files: Mutex::new(FileIndex::default()),
            claude_home: OnceLock::new(),
            skills: Mutex::new(HashMap::new()),
            claude_listed: Mutex::new(HashMap::new()),
            titles: Mutex::new(JoinSet::new()),
            clock: crate::jobs::Clock::default(),
        }))
    }

    /// Where to look for Claude Code's own skills and plugins. Without it,
    /// only a project's skills are found.
    pub fn set_claude_home(&self, home: PathBuf) {
        let _ = self.0.claude_home.set(home);
    }

    /// A receiver for all notifications from now on.
    pub fn notices(&self) -> broadcast::Receiver<Notice> {
        self.0.tx.subscribe()
    }

    /// The file of a skill found earlier in the project, or an empty path
    /// when it hasn't been looked for since crewd started.
    pub(crate) fn cached_skill_path(
        &self,
        project: ProjectId,
        runtime: &Runtime,
        name: &str,
    ) -> String {
        let skills = self.0.skills.lock().unwrap_or_else(PoisonError::into_inner);
        skills
            .get(&project)
            .and_then(|cache| {
                cache
                    .claude
                    .iter()
                    .chain(cache.codex.iter().flatten())
                    .find(|f| f.runtime == *runtime && f.name == name)
                    .map(|f| f.path.clone())
            })
            .unwrap_or_default()
    }

    /// The time scheduled jobs go by.
    pub(crate) fn clock(&self) -> &crate::jobs::Clock {
        &self.0.clock
    }

    pub(crate) fn db(&self) -> MutexGuard<'_, Db> {
        self.0.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How long one minute of a run's time limit lasts.
    pub(crate) fn minute(&self) -> Duration {
        self.0.minute
    }

    /// Expires old stopped calls now and then every twenty-fourth of
    /// their lifetime, an hour in real time.
    pub(crate) fn sweep_blocked(&self) -> tokio::task::JoinHandle<()> {
        let svc = self.clone();
        let every = (self.blocked_ttl() / 24).max(Duration::from_millis(10));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                svc.expire_old_blocked();
            }
        })
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

    pub(crate) fn changes(&self) -> MutexGuard<'_, ChangeCache> {
        self.0
            .changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the search reader, or on the main store when there is
    /// none.
    pub(crate) fn with_reader<T>(&self, f: impl FnOnce(&Store) -> T) -> T {
        match &self.0.reader {
            Some(reader) => f(&reader.lock().unwrap_or_else(PoisonError::into_inner)),
            None => f(&self.db().store),
        }
    }

    pub(crate) fn files(&self) -> MutexGuard<'_, FileIndex> {
        self.0.files.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets the thread's cached change counts and file list.
    pub(crate) fn changes_may_differ(&self, thread: ThreadId) {
        {
            let mut cache = self.changes();
            cache.invalidate(thread);
            cache.forget_edits(thread);
        }
        self.files_may_differ(thread);
    }

    /// Resolves the next time counts were worked out.
    pub(crate) fn counted(&self) -> tokio::sync::futures::Notified<'_> {
        self.0.counted.notified()
    }

    pub(crate) fn notify_counted(&self) {
        self.0.counted.notify_waiters();
    }

    pub(crate) fn count_slots(&self) -> Arc<tokio::sync::Semaphore> {
        self.0.count_slots.clone()
    }

    pub(crate) fn count_delay(&self) -> Duration {
        Duration::from_millis(
            self.0
                .count_delay_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Makes each change count take `delay` longer, for tests.
    pub fn set_count_delay(&self, delay: Duration) {
        let ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self.0
            .count_delay_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Drops the thread's counts when a live run finished editing or
    /// writing a file, at most once every few seconds.
    fn note_edit(&self, thread: ThreadId, event: &Event) {
        match event {
            Event::ToolCall {
                id,
                kind: ToolKind::Edit | ToolKind::Create,
                ..
            } => {
                self.changes().edit_started(thread, id.clone());
            }
            Event::ToolResult { id, .. } => {
                let live = {
                    let mut cache = self.changes();
                    if !cache.edit_finished(thread, id) {
                        return;
                    }
                    cache.invalidate_live(thread, std::time::Instant::now())
                };
                match live {
                    LiveEdit::Dropped => self.thread_overview_stale(thread),
                    LiveEdit::Held { wait, first: true } => {
                        let svc = self.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(wait).await;
                            let dropped =
                                svc.changes().apply_due(thread, std::time::Instant::now());
                            if dropped {
                                svc.thread_overview_stale(thread);
                            }
                        });
                    }
                    LiveEdit::Held { first: false, .. } => {}
                }
            }
            _ => {}
        }
    }

    /// Tells `SubscribeAll` clients that the overview of `thread`'s
    /// channel changed.
    pub(crate) fn thread_overview_stale(&self, thread: ThreadId) {
        let channel = match self.db().store.thread(thread) {
            Ok(t) => t.channel,
            Err(e) => {
                log(format!("couldn't read thread {thread}: {e}"));
                return;
            }
        };
        self.overview_stale(channel);
    }

    /// Tells `SubscribeAll` clients that the channel's overview changed.
    pub(crate) fn overview_stale(&self, channel: ChannelId) {
        let mut db = self.db();
        self.publish_global(&mut db, ServerMsg::ChannelOverviewStale { channel });
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
    /// `Commit`, `Discard` and `ChannelOverview` run git, and `Runtimes`,
    /// `RunJobNow` and `TestJob` may run the agent CLIs, so they can take
    /// a while.
    pub async fn handle(&self, req: Request) -> Reply {
        self.refuse_removed(&req)?;
        match req {
            Request::ThreadDiff { thread } => self.thread_changes(thread).await,
            Request::Commit {
                thread,
                repo,
                message,
            } => self.commit_in(thread, repo, &message).await,
            Request::Discard {
                thread,
                repos,
                delete_branch,
            } => self.discard_in(thread, repos, delete_branch).await,
            Request::InspectFolder { path } => self.inspect_folder(path).await,
            Request::CreateProject {
                name,
                root,
                main_branch,
                work_mode,
                copy_files,
                setup_command,
            } => {
                let svc = self.clone();
                let made = blocking(move || {
                    svc.create_project(NewProject {
                        name: &name,
                        root: Path::new(&root),
                        main: &main_branch,
                        work_mode,
                        copy_files: &copy_files,
                        setup_command: setup_command.as_deref(),
                    })
                })
                .await?;
                json(made)
            }
            Request::ChannelOverview { channel } => self.channel_overview(channel).await,
            Request::ThreadSummaries { project, channels } => {
                self.thread_summaries(project, channels).await
            }
            Request::Runtimes => self.runtimes().await,
            Request::Skills {
                project,
                force_reload,
            } => self.skills(project, force_reload).await,
            Request::Search {
                query,
                project,
                limit,
            } => self.search(query, project, limit).await,
            Request::ProjectOverview { project, light } => {
                self.project_overview(project, light).await
            }
            Request::AgentProfile { agent } => self.agent_profile(agent).await,
            Request::RunJobNow { job } => json(self.run_job_now(job).await?),
            Request::TestJob { job } => json(self.test_job(job).await?),
            other => self.handle_now(other),
        }
    }

    /// Refuses a write into a removed project. The writes that start runs
    /// check again under the lock they write with.
    fn refuse_removed(&self, req: &Request) -> Result<(), String> {
        let db = self.db();
        let open = match req {
            Request::PostMessage { thread, .. }
            | Request::RenameThread { thread, .. }
            | Request::ArchiveThread { thread, .. }
            | Request::SetThreadHandoff { thread, .. }
            | Request::AllowMoreHandoffs { thread, .. }
            | Request::Commit { thread, .. }
            | Request::Discard { thread, .. } => db.store.ensure_thread_open(*thread),
            Request::CreateThread { channel, .. }
            | Request::UpdateChannel { channel, .. }
            | Request::SetChannelHandoff { channel, .. } => db.store.ensure_channel_open(*channel),
            Request::CreateChannel { project, .. }
            | Request::UpdateProject { project, .. }
            | Request::RenameProject { project, .. }
            | Request::RemoveProject { project }
            | Request::PinProject { project, .. } => db.store.ensure_project_open(*project),
            _ => Ok(()),
        };
        open.map_err(|e| e.to_string())
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
            Request::CreateChannel { project, name } => {
                let mut db = self.db();
                db.store
                    .ensure_project_open(project)
                    .map_err(|e| e.to_string())?;
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
            Request::CreateThread {
                channel,
                title,
                first_line,
            } => json(
                self.create_thread(channel, &title, !first_line)
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
            Request::ArchiveThread { thread, archived } => {
                let mut db = self.db();
                db.store
                    .set_thread_archived(thread, archived)
                    .map_err(|e| e.to_string())?;
                self.thread_changed(&mut db, thread)?;
                // An archived thread's job stops for good.
                if archived {
                    self.delete_thread_job(&mut db, thread)?;
                }
                drop(db);
                // An archived thread's runs are no longer undone.
                if archived {
                    self.forget_thread_snapshots(thread);
                }
                Ok(serde_json::Value::Null)
            }
            Request::UpdateChannel {
                channel,
                name,
                description,
            } => {
                self.update_channel(channel, &name, description.trim())?;
                Ok(serde_json::Value::Null)
            }
            Request::PostMessage {
                thread,
                body,
                skill,
                model_override,
            } => json(self.post_message(thread, &body, skill, model_override)?),
            Request::SetSkillShown {
                runtime,
                source,
                name,
                shown,
                project,
            } => {
                let key = match (&source, project) {
                    (SkillSource::Project, None) => {
                        return Err("Say which project the skill is in.".into());
                    }
                    // Only a project's own skill's key holds the project.
                    (_, project) => {
                        skill_key(&runtime, &source, &name, project.unwrap_or_default())
                    }
                };
                self.db()
                    .store
                    .set_skill_shown(&key, shown)
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::Value::Null)
            }
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
            Request::SetLimits { limits } => {
                self.set_limits(limits)?;
                Ok(serde_json::Value::Null)
            }
            Request::SetTitleSettings { titles } => {
                let mut db = self.db();
                db.store
                    .set_title_settings(&titles)
                    .map_err(|e| e.to_string())?;
                self.publish_global(&mut db, ServerMsg::TitleSettingsChanged { titles });
                Ok(serde_json::Value::Null)
            }
            Request::RunHistory { before, limit } => json(
                self.db()
                    .store
                    .run_history(before, limit)
                    .map_err(|e| e.to_string())?,
            ),
            Request::UsageSince { since_ms } => json(
                self.db()
                    .store
                    .usage_since(since_ms, now_ms())
                    .map_err(|e| e.to_string())?,
            ),
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
            | Request::CreateProject { .. }
            | Request::InspectFolder { .. }
            | Request::Commit { .. }
            | Request::Discard { .. }
            | Request::ChannelOverview { .. }
            | Request::ThreadSummaries { .. }
            | Request::Search { .. }
            | Request::ProjectOverview { .. }
            | Request::AgentProfile { .. }
            | Request::RunJobNow { .. }
            | Request::TestJob { .. }
            | Request::Skills { .. }
            | Request::Runtimes => Err("This request is slow and is handled elsewhere.".into()),
            Request::RenameProject { project, name } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err("Give the project a name.".into());
                }
                let mut db = self.db();
                db.store
                    .rename_project(project, name)
                    .map_err(|e| e.to_string())?;
                self.project_changed(&mut db, project)?;
                Ok(serde_json::Value::Null)
            }
            Request::RemoveProject { project } => {
                self.remove_project(project)?;
                Ok(serde_json::Value::Null)
            }
            Request::PinProject { project, pinned } => {
                let mut db = self.db();
                db.store
                    .pin_project(project, pinned)
                    .map_err(|e| e.to_string())?;
                self.project_changed(&mut db, project)?;
                Ok(serde_json::Value::Null)
            }
            Request::CreateJob { job } => json(self.create_job(job)?),
            Request::UpdateJob { job, patch } => {
                self.update_job(job, patch)?;
                Ok(serde_json::Value::Null)
            }
            Request::DeleteJob { job } => {
                self.delete_job(job)?;
                Ok(serde_json::Value::Null)
            }
            Request::SetJobEnabled { job, enabled } => {
                self.set_job_enabled(job, enabled)?;
                Ok(serde_json::Value::Null)
            }
            Request::Report {
                run,
                found,
                summary,
            } => {
                self.report(run, found, summary)?;
                Ok(serde_json::Value::Null)
            }
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
    pub(crate) fn project_changed(&self, db: &mut Db, project: ProjectId) -> Result<(), String> {
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

    /// The skills of both CLIs in the project. Found once and kept until
    /// `force_reload`. Codex ones are left out if it isn't installed or
    /// signed in. A Codex listing that failed or timed out isn't kept, so
    /// the next request tries it again. Plugin skills of Claude Code that
    /// a run's init line didn't list are left out, once a run has told.
    pub(crate) async fn skills(&self, project: ProjectId, force_reload: bool) -> Reply {
        let cached = (!force_reload)
            .then(|| self.0.skills.lock().unwrap().get(&project).cloned())
            .flatten();
        let root = {
            let db = self.db();
            db.store.project(project).map_err(|e| e.to_string())?.root
        };
        let claude = match &cached {
            Some(cached) => cached.claude.clone(),
            None => {
                let home = self.0.claude_home.get().cloned();
                let scan_root = root.clone();
                tokio::task::spawn_blocking(move || {
                    crate::skills::scan_claude(home.as_deref(), &scan_root)
                })
                .await
                .map_err(|e| e.to_string())?
            }
        };
        let codex = match cached.and_then(|c| c.codex) {
            Some(codex) => Some(codex),
            None => {
                let bin = crate::runtimes::codex_bin(&self.0.bins);
                match crate::runtimes::find_bin(&bin) {
                    // Not installed: a definitive answer.
                    None => Some(vec![]),
                    Some(path) => match crate::runtimes::codex_signed_in(&path).await {
                        Ok(Some(true)) => {
                            crate::skills::scan_codex(&bin, &root, force_reload).await
                        }
                        // Signed out: a definitive answer.
                        Ok(Some(false)) => Some(vec![]),
                        // Couldn't tell: try again next time.
                        _ => None,
                    },
                }
            }
        };
        self.0.skills.lock().unwrap().insert(
            project,
            SkillCache {
                claude: claude.clone(),
                codex: codex.clone(),
            },
        );
        let listed = self.0.claude_listed.lock().unwrap().get(&project).cloned();
        let db = self.db();
        let list: Vec<SkillInfo> = claude
            .iter()
            .filter(|f| match (&f.source, &listed) {
                (SkillSource::Plugin(_), Some(listed)) => listed.contains(&f.name),
                _ => true,
            })
            .chain(codex.iter().flatten())
            .map(|f| {
                let key = skill_key(&f.runtime, &f.source, &f.name, project);
                f.info(db.store.skill_shown(&key).unwrap_or(true))
            })
            .collect();
        json(list)
    }

    /// Tells `SubscribeAll` clients the channel as it is now.
    fn channel_changed(&self, db: &mut Db, channel: ChannelId) -> Result<(), String> {
        let channel = db.store.channel(channel).map_err(|e| e.to_string())?;
        self.publish_global(db, ServerMsg::ChannelChanged { channel });
        Ok(())
    }

    /// Renames the channel and sets its description, once the name is
    /// valid and no other channel of its project has it.
    fn update_channel(
        &self,
        channel: ChannelId,
        name: &str,
        description: &str,
    ) -> Result<(), String> {
        if description.chars().count() > MAX_DESCRIPTION {
            return Err(format!(
                "Keep the description under {MAX_DESCRIPTION} characters."
            ));
        }
        let mut db = self.db();
        let stored = db.store.channel(channel).map_err(|e| e.to_string())?;
        // A name kept as it is stays valid, even one from before the rules.
        let typed = name.trim();
        let name = if typed.strip_prefix('#').unwrap_or(typed) == stored.name {
            stored.name.clone()
        } else {
            crew_core::slug::channel_name(name)?
        };
        let project = db
            .store
            .project(stored.project)
            .map_err(|e| e.to_string())?;
        let taken = db
            .store
            .channels(stored.project)
            .map_err(|e| e.to_string())?
            .iter()
            .any(|c| c.id != channel && c.name.eq_ignore_ascii_case(&name));
        if taken {
            return Err(format!("#{name} already exists in {}.", project.name));
        }
        db.store
            .update_channel(channel, &name, description)
            .map_err(|e| e.to_string())?;
        self.channel_changed(&mut db, channel)
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
    /// A `chosen` title is never replaced by a generated one.
    fn create_thread(
        &self,
        channel: ChannelId,
        title: &str,
        chosen: bool,
    ) -> Result<ThreadId, StoreError> {
        let mut db = self.db();
        db.store.ensure_channel_open(channel)?;
        let id = db.store.create_thread_titled(channel, title, chosen)?;
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
        self.pause_agent_jobs(&mut db, agent)
    }

    /// Adds a project, unless another one uses the folder. A removed
    /// project with the folder comes back instead, the most recently
    /// removed one, with its channels and threads. Folders are compared
    /// with links resolved, which reads the disk, so not under a lock.
    /// Any folder works; what it holds decides the default work mode.
    fn create_project(&self, new: NewProject) -> Result<ProjectId, String> {
        let NewProject {
            name,
            root,
            main,
            work_mode,
            copy_files,
            setup_command,
        } = new;
        if !root.is_dir() {
            return Err("Folder not found.".into());
        }
        let kind = detect_kind(root);
        let mode = work_mode.unwrap_or_else(|| kind.default_mode());
        kind.allows(mode)?;
        let main = if !main.is_empty() || kind != ProjectKind::Repo {
            main.to_string()
        } else {
            worktree::current_branch(root).unwrap_or_else(|| "HEAD".into())
        };
        let setup_command = setup_command.map(str::trim).filter(|c| !c.is_empty());
        let err = |e: StoreError| e.to_string();
        let _adding = self
            .0
            .adding_project
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let wanted = same_folder(root);
        let (active, removed) = {
            let db = self.db();
            (
                db.store.projects().map_err(err)?,
                db.store.removed_projects().map_err(err)?,
            )
        };
        if let Some(taken) = active.iter().find(|p| same_folder(&p.root) == wanted) {
            return Err(format!("{} already uses this folder.", taken.name));
        }
        let revived = removed.iter().find(|p| same_folder(&p.root) == wanted);
        let mut db = self.db();
        let Some(old) = revived else {
            let project = db
                .store
                .in_transaction(|s| {
                    let project = s.create_project(name, root, &main)?;
                    s.set_project_kind(project, &kind)?;
                    s.set_project_mode(project, mode)?;
                    s.set_project_setup(project, setup_command, copy_files)?;
                    Ok(project)
                })
                .map_err(err)?;
            self.project_changed(&mut db, project)?;
            return Ok(project);
        };
        db.store
            .in_transaction(|s| {
                s.restore_project(old.id, name)?;
                s.set_project_kind(old.id, &kind)?;
                if work_mode.is_some() {
                    s.set_project_mode(old.id, mode)?;
                }
                Ok(())
            })
            .map_err(err)?;
        self.project_changed(&mut db, old.id)?;
        for channel in db.store.channels(old.id).map_err(err)? {
            self.channel_changed(&mut db, channel.id)?;
        }
        for thread in db.store.project_threads(old.id).map_err(err)? {
            self.publish_global(&mut db, ServerMsg::ThreadChanged { thread });
        }
        // Runs that ended lately, as a snapshot would bring them.
        let since = now_ms() - RECENT_RUNS_MS;
        for run in db.store.project_runs(old.id).map_err(err)? {
            if run.ended_at.is_some_and(|e| e >= since) {
                self.publish_status(&mut db, run.thread, run.id, run.status.clone());
            }
        }
        Ok(old.id)
    }

    /// Removes the project from Crew. Refused while one of its runs is
    /// live or a worktree change is under way. In one transaction, its
    /// queued runs are dropped with a note, its pending approvals closed
    /// with a note, and the project marked removed. Its threads and runs
    /// stay for history; its terminal sessions are no longer listed.
    /// Nothing on disk is touched.
    fn remove_project(&self, project: ProjectId) -> Result<(), String> {
        let err = |e: StoreError| e.to_string();
        let (dirs, threads) = {
            let mut state = self.state();
            let mut db = self.db();
            let stored = db.store.project(project).map_err(err)?;
            let threads = db.store.project_threads(project).map_err(err)?;
            let ids: std::collections::HashSet<ThreadId> = threads.iter().map(|t| t.id).collect();
            if state.live.values().any(|(t, _)| ids.contains(t)) {
                return Err(format!("Stop the agents in {} first.", stored.name));
            }
            if ids.iter().any(|t| state.scheduler.is_active(*t)) {
                return Err(format!(
                    "Wait for the worktree change in {} to finish.",
                    stored.name
                ));
            }
            let pending: Vec<StoredApproval> = db
                .store
                .pending_approvals(None)
                .map_err(err)?
                .into_iter()
                .filter(|a| ids.contains(&a.thread))
                .collect();
            let queue = state.scheduler.clear();
            let dropped: Vec<&Job> = queue.iter().filter(|j| ids.contains(&j.thread)).collect();
            let saved = db.store.in_transaction(|s| {
                let mut notes = Vec::new();
                let mut noted = Vec::new();
                for job in &dropped {
                    s.set_run_status(job.run, RunStatus::Stopped)?;
                    if !noted.contains(&job.thread) {
                        noted.push(job.thread);
                    }
                }
                for thread in noted {
                    let body = "Stopped before starting.";
                    notes.push((thread, s.add_message(thread, Author::System, body, None)?));
                }
                let mut closed = Vec::new();
                for a in &pending {
                    closed.push(s.decide_approval(a.id, ApprovalStatus::Cancelled, None)?);
                    let id = s.add_message(a.thread, Author::System, PROJECT_GONE, Some(a.run))?;
                    notes.push((a.thread, id));
                }
                s.remove_project(project)?;
                Ok((notes, closed))
            });
            let (notes, closed) = match saved {
                Ok(saved) => saved,
                Err(e) => {
                    for job in queue {
                        state.scheduler.enqueue(job);
                    }
                    return Err(e.to_string());
                }
            };
            let dropped: Vec<Job> = dropped.into_iter().cloned().collect();
            for job in queue {
                if !ids.contains(&job.thread) {
                    state.scheduler.enqueue(job);
                }
            }
            for job in &dropped {
                state.pending.remove(&job.run);
                state.retry_rules.remove(&job.run);
                self.publish_status(&mut db, job.thread, job.run, RunStatus::Stopped);
            }
            for approval in closed {
                let thread = approval.thread;
                self.publish(&mut db, thread, ServerMsg::Approval { thread, approval });
            }
            for (thread, id) in notes {
                if let Err(e) = self.publish_message(&mut db, thread, id) {
                    log(format!("couldn't send a note of thread {thread}: {e}"));
                }
            }
            let mut terminal = self.terminal();
            for id in terminal.remove_project(project) {
                self.publish_global(&mut db, ServerMsg::TerminalSessionRemoved { id });
            }
            drop(terminal);
            self.publish_global(&mut db, ServerMsg::ProjectRemoved { project });
            // Its channels are gone, and so are the jobs in them.
            for thread in &threads {
                if let Err(e) = self.delete_thread_job(&mut db, thread.id) {
                    log(format!(
                        "couldn't delete the job of thread {}: {e}",
                        thread.id
                    ));
                }
            }
            let mut dirs: Vec<PathBuf> = crate::folders::project_checkouts(&stored)
                .into_iter()
                .map(|(dir, _)| dir)
                .collect();
            for wt in threads.iter().filter_map(|t| t.worktree.as_ref()) {
                dirs.extend(
                    crate::folders::worktree_checkouts(wt)
                        .into_iter()
                        .map(|(dir, _)| dir),
                );
            }
            (dirs, ids)
        };
        {
            let mut index = self.files();
            for dir in &dirs {
                index.forget(dir);
            }
        }
        {
            let mut cache = self.changes();
            for thread in threads {
                cache.invalidate(thread);
                cache.forget_edits(thread);
            }
        }
        self.rescan_terminal();
        Ok(())
    }

    /// Removes the thread's worktree, uncommitted changes included. Its
    /// branch goes too, unless it has commits beyond the worktree's base
    /// and `delete_branch` is off: then it is kept and the thread is told.
    /// The thread is claimed by the caller, which releases it.
    pub(crate) async fn discard_claimed(
        &self,
        thread: ThreadId,
        wt: ThreadWorktree,
        delete_branch: bool,
    ) -> Result<(), String> {
        let root = self
            .db()
            .store
            .project_of_thread(thread)
            .map_err(|e| e.to_string())?
            .root;
        let (path, branch) = (wt.path.clone(), wt.branch.clone());
        let result = blocking(move || {
            let keep = !delete_branch && worktree::has_commits_beyond(&root, &wt.branch, &wt.base)?;
            worktree::remove(&root, &wt.path, &wt.branch, !keep)?;
            Ok::<_, worktree::WorktreeError>(keep)
        })
        .await;
        self.changes_may_differ(thread);
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
        result.map_err(|e| e.to_string())?;
        cleared
    }

    /// Holds the thread so no run starts there while its changes are
    /// committed or dropped, and returns its worktree if it has one.
    /// Refuses while a run in the thread is live, saying `busy` of its
    /// agent's name.
    pub(crate) fn claim_for_change(
        &self,
        thread: ThreadId,
        busy: impl FnOnce(&str) -> String,
    ) -> Result<Option<ThreadWorktree>, String> {
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
            return Err(busy(name));
        }
        let wt = db.store.thread(thread).map_err(|e| e.to_string())?.worktree;
        if !state.scheduler.claim(thread) {
            return Err("Another change to this thread's worktree is in progress.".into());
        }
        Ok(wt)
    }

    /// Lets runs start in the thread again.
    pub(crate) fn release_change(&self, thread: ThreadId) {
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
                details: Box::new(run),
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
        let jobs = db.store.jobs()?;
        let mut job_runs = Vec::new();
        let mut job_notes = Vec::new();
        for job in &jobs {
            let runs = db.store.job_runs(job.id, crew_proto::SNAPSHOT_JOB_RUNS)?;
            let ids: HashSet<RunId> = runs.iter().map(|r| r.id).collect();
            if !ids.is_empty() {
                job_notes.extend(db.store.messages(job.thread)?.into_iter().filter(|m| {
                    m.author == Author::System && m.run.is_some_and(|r| ids.contains(&r))
                }));
            }
            job_runs.extend(runs);
        }
        let msg = ServerMsg::Snapshot {
            projects,
            channels,
            threads,
            agents: db.store.all_agents()?,
            runs,
            approvals,
            terminal_sessions: self.terminal().sessions(),
            limits: db.store.limits()?,
            titles: db.store.title_settings()?,
            jobs,
            job_history: Box::new(crew_proto::JobHistory {
                job_runs,
                job_notes,
            }),
        };
        Ok((msg, db.serial))
    }

    /// Saves the message, then queues a run for each mentioned agent. A
    /// message that mentions no one goes to the agent that ran last in
    /// the thread, if none is queued or working there; otherwise it is
    /// context for the next run.
    pub fn post_message(
        &self,
        thread: ThreadId,
        body: &str,
        skill: Option<SkillRef>,
        model_override: Option<String>,
    ) -> Result<MessageId, String> {
        let model_override = tidy_model_override(model_override)?;
        let (id, title_run) = {
            let mut state = self.state();
            let mut db = self.db();
            db.store
                .ensure_thread_open(thread)
                .map_err(|e| e.to_string())?;
            let earlier = db.store.messages(thread).map_err(|e| e.to_string())?;
            let agents = db.store.agents().map_err(|e| e.to_string())?;

            let names: Vec<&str> = agents.iter().map(|(_, a)| a.name.as_str()).collect();
            let (known, unknown) = parse_mentions(body, &names);
            let mut called: Vec<AgentId> = known
                .iter()
                .filter_map(|name| agents.iter().find(|(_, a)| a.name == *name))
                .map(|(agent, _)| *agent)
                .collect();
            // A skill picked to run with one agent goes to it alone; the
            // agents mentioned stay mentioned.
            if let Some(agent) = skill.as_ref().and_then(|s| s.agent) {
                if !agents.iter().any(|(id, _)| *id == agent) {
                    return Err("That agent no longer exists. Pick another.".into());
                }
                called = vec![agent];
            } else if called.is_empty() && unknown.is_empty() {
                // A reply to nobody in particular goes to whoever answered
                // last, unless someone is still at work.
                called.extend(
                    self.last_agent(&db, thread, &agents)
                        .map_err(|e| e.to_string())?,
                );
            }
            // A skill goes to an agent that uses its CLI. Nothing is saved
            // otherwise.
            if let Some(skill) = &skill {
                let runtime = match skill.runtime {
                    Runtime::ClaudeCode => "Claude Code",
                    Runtime::Codex => "Codex",
                };
                if called.is_empty() {
                    return Err(format!(
                        "Mention an agent that uses {runtime} to run /{}.",
                        skill.name
                    ));
                }
                let wrong = called.iter().any(|agent| {
                    agents
                        .iter()
                        .any(|(id, a)| id == agent && a.runtime != skill.runtime)
                });
                if wrong {
                    return Err(format!(
                        "/{} runs on {runtime}. Pick an agent that uses it.",
                        skill.name
                    ));
                }
            }
            let id = self
                .add_message(&mut db, thread, Author::You, body, None)
                .map_err(|e| e.to_string())?;
            let title_run = if earlier.is_empty() {
                self.start_title(&mut db, thread, body)
            } else {
                None
            };
            let context = render_context(
                &earlier,
                &db.store.agents_with_deleted().map_err(|e| e.to_string())?,
            );
            let fresh = if context.is_empty() {
                body.to_string()
            } else {
                format!("{context}\n\n{body}")
            };
            for agent in &called {
                let run = db
                    .store
                    .create_run(thread, *agent)
                    .map_err(|e| e.to_string())?;
                if let Some(skill) = &skill {
                    db.store
                        .set_run_skill(run, &skill.name)
                        .map_err(|e| e.to_string())?;
                }
                db.store
                    .set_run_request(run, id, model_override.as_deref())
                    .map_err(|e| e.to_string())?;
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
                        skill: skill.clone(),
                        model_override: model_override.clone(),
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
                )
                .map_err(|e| e.to_string())?;
            }
            (id, title_run)
        };
        if let Some(model) = title_run {
            self.spawn_title(thread, body.to_string(), model);
        }
        self.start_ready();
        Ok(id)
    }

    /// Marks the thread's title as being generated, when titles are and
    /// it is still its first line, and gives the model to generate it on.
    /// A message with no text, such as a skill alone, keeps its title.
    fn start_title(&self, db: &mut Db, thread: ThreadId, body: &str) -> Option<TitleModel> {
        if body.trim().is_empty() || self.0.bins.no_title_runs {
            return None;
        }
        let started = db.store.title_settings().and_then(|titles| {
            Ok(match titles {
                TitleSettings {
                    mode: TitleMode::Generated,
                    model,
                } => db.store.start_title(thread)?.then_some(model),
                _ => None,
            })
        });
        match started {
            Ok(Some(model)) => {
                self.thread_changed_or_log(db, thread);
                Some(model)
            }
            Ok(None) => None,
            Err(e) => {
                log(format!("couldn't start a title run: {e}"));
                None
            }
        }
    }

    /// Generates the thread's title from its first message, apart from
    /// any run. The first line stays if it fails or you renamed the
    /// thread meanwhile.
    fn spawn_title(&self, thread: ThreadId, message: String, model: TitleModel) {
        let svc = self.clone();
        let mut titles = self.0.titles.lock().unwrap_or_else(PoisonError::into_inner);
        // Forget the ones that ended.
        while titles.try_join_next().is_some() {}
        titles.spawn(async move {
            let title = crate::titles::generate(&svc.0.bins, model, &message).await;
            let mut db = svc.db();
            let changed = match &title {
                Some(title) => db.store.set_generated_title(thread, title),
                None => db.store.keep_first_line(thread),
            };
            match changed {
                Ok(true) => svc.thread_changed_or_log(&mut db, thread),
                Ok(false) => {}
                Err(e) => log(format!("couldn't save the thread's title: {e}")),
            }
        });
    }

    /// The agent that ran last in `thread`, if it still exists and no run
    /// there is queued or working.
    fn last_agent(
        &self,
        db: &Db,
        thread: ThreadId,
        agents: &[(AgentId, AgentSpec)],
    ) -> Result<Option<AgentId>, StoreError> {
        let runs = db.store.runs(RunFilter {
            status: None,
            thread: Some(thread),
        })?;
        if runs
            .iter()
            .any(|r| matches!(r.status, RunStatus::Queued | RunStatus::Working))
        {
            return Ok(None);
        }
        Ok(runs
            .iter()
            .rev()
            .map(|r| r.agent)
            .find(|agent| agents.iter().any(|(id, _)| id == agent)))
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
                state.retry_rules.remove(&run);
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
            state.retry_rules.remove(&job.run);
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
        state.retry_rules.clear();
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
            state.retry_rules.clear();
            if let Err(e) = self.note_dropped(&dropped, RunStatus::Interrupted) {
                log(format!("couldn't record the queued runs: {e}"));
            }
            std::mem::take(&mut state.watchers)
        };
        // Title runs are dropped, their processes with them.
        self.0
            .titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .abort_all();
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
    /// `claude` process starts, after any worktree setup. An agent with no
    /// limit gets no timer.
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
        // 0 is no limit.
        if minutes == 0 {
            return;
        }
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

    /// Forgets a run that is no longer live, its time limit, and the
    /// folder it was writing in.
    fn end_live(state: &mut State, run: RunId) {
        state.live.remove(&run);
        state.writers.retain(|_, writer| *writer != run);
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

    /// Saves the limits, applies them to the scheduler, tells every client
    /// and starts queued jobs the new limits allow. Live runs are never
    /// stopped by a lower cap or a pause.
    pub(crate) fn set_limits(&self, limits: Limits) -> Result<(), String> {
        limits.validate()?;
        {
            let mut state = self.state();
            let mut db = self.db();
            db.store.set_limits(&limits).map_err(|e| e.to_string())?;
            state.scheduler.set_cap(limits.max_runs as usize);
            state.scheduler.set_paused(limits.paused);
            self.publish_global(&mut db, ServerMsg::LimitsChanged { limits });
        }
        self.start_ready();
        Ok(())
    }

    /// Starts queued jobs while the scheduler allows.
    pub(crate) fn start_ready(&self) {
        let mut state = self.state();
        if state.shutting_down {
            return;
        }
        loop {
            // In place, one agent writes in a folder at a time.
            let job = {
                let State {
                    scheduler,
                    writers,
                    changing,
                    ..
                } = &mut *state;
                let db = self.db();
                scheduler.next_ready_where(|job| self.may_write(&db, writers, changing, job))
            };
            let Some(job) = job else { break };
            let writes_in = self.in_place_writer(&self.db(), &job);
            if let Some(project) = writes_in {
                state.writers.insert(project, job.run);
            }
            let pending = state.pending.remove(&job.run);
            // Forget watchers that are done, so the set stays small.
            while state.watchers.try_join_next().is_some() {}
            // A run that writes in place first notes what the folder holds.
            if self.needs_worktree(&job) || writes_in.is_some() {
                self.begin_setup(&mut state, job, pending);
            } else if let Some(watch) = self.launch(&mut state, job, pending, true) {
                state.watchers.spawn(watch);
            }
        }
        self.update_waiting(&state);
    }

    /// Whether the job's agent may write, its project gives each thread
    /// its own copy, and the thread has no worktree, or one whose setup
    /// didn't finish. Store errors say no, so `prepare` reports them.
    fn needs_worktree(&self, job: &Job) -> bool {
        let db = self.db();
        let writes = db.store.agents().is_ok_and(|agents| {
            agents.iter().any(|(id, spec)| {
                *id == job.agent && spec.permission_mode != PermissionMode::ReadOnly
            })
        });
        let own_copy = db
            .store
            .project_of_thread(job.thread)
            .is_ok_and(|p| p.work_mode == WorkMode::OwnCopy);
        writes
            && own_copy
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
            match db.store.begin_run(job.run) {
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
        let extra = state.retry_rules.remove(&job.run).unwrap_or_default();
        let mut db = self.db();
        match self.prepare(&mut db, &job, pending, announce, &extra) {
            Ok((name, mut inv)) => {
                drop(db);
                // So the user's hooks can tell a Crew run apart.
                let env = match &mut inv {
                    Invocation::Claude(inv) => &mut inv.env,
                    Invocation::Codex(inv) => &mut inv.env,
                };
                env.push(("CREW_RUN_ID".to_string(), job.run.to_string()));
                let (handle, rx, join) = match inv {
                    Invocation::Claude(inv) => spawn_run(inv),
                    Invocation::Codex(inv) => {
                        spawn_codex_run(*inv, self.codex_hooks(job.thread, job.run))
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
        let in_place = self.in_place_writer(&self.db(), &job).is_some();
        let prepared = if in_place {
            self.take_snapshot(&job).await;
            Ok(())
        } else {
            self.set_up_worktree(&job, stop_rx).await
        };
        let next = match prepared {
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

    pub(crate) fn agent_name(&self, agent: AgentId) -> String {
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
                // Removed ones too, so slugs stay as they were.
                let projects = db.store.all_projects()?;
                let mut thread = db.store.thread(job.thread)?;
                // The branch keeps the name of the first line, even once
                // a title was generated.
                thread.title = db.store.branch_title(job.thread)?;
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
                let (id, root, main, title) = (
                    project.id,
                    project.root.clone(),
                    project.main_branch.clone(),
                    thread.title,
                );
                let wt = blocking(move || {
                    // A fresh look, so a repo added since is in.
                    let kind = svc.refresh_kind(id).unwrap_or_default();
                    let repos = match &kind {
                        ProjectKind::Plain => return Err(NEEDS_REPO.to_string()),
                        ProjectKind::Repo => vec![String::new()],
                        ProjectKind::MultiRepo { repos } => repos.clone(),
                    };
                    let _held = svc.0.slugs.lock().unwrap_or_else(PoisonError::into_inner);
                    let taken = |slug: &str| {
                        dir.join(slug).exists()
                            || repos.iter().any(|repo| {
                                worktree::branch_exists(&root.join(repo), &format!("crew/{slug}"))
                                    .unwrap_or(false)
                            })
                    };
                    let slug = slugify(&title, &taken);
                    let path = dir.join(&slug);
                    let branch = format!("crew/{slug}");
                    if kind != ProjectKind::Repo {
                        return crate::folders::make_repo_worktrees(&root, &repos, &path, &branch);
                    }
                    let base = worktree::create(&root, &main, &path, &branch)
                        .map_err(|e| e.to_string())?;
                    Ok(ThreadWorktree {
                        path,
                        branch,
                        base,
                        ready: false,
                        repos: Vec::new(),
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
        self.changes_may_differ(thread);
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
        extra_allowed: &[String],
    ) -> Result<(String, Invocation), StoreError> {
        db.store.ensure_thread_open(job.thread)?;
        let agents = db.store.agents_with_deleted()?;
        let spec = db.store.agent(job.agent)?;
        if spec.deleted_at.is_some() {
            return Err(StoreError::NotFound("agent"));
        }
        let mut spec = spec.spec;
        // The override is for this run alone; the agent keeps its model.
        let skill = pending.as_ref().and_then(|p| p.skill.clone());
        if let Some(model) = pending.as_ref().and_then(|p| p.model_override.clone()) {
            spec.model = model;
        }
        let cwd = db.store.thread_cwd(job.thread)?;
        let project = db.store.project_of_thread(job.thread)?;
        // A thread folder of repo worktrees leaves plain files out; the
        // agent may read them in the original folder.
        let worktree = db.store.thread(job.thread)?.worktree;
        let read_only_root = worktree
            .as_ref()
            .is_some_and(|wt| !wt.repos.is_empty())
            .then(|| project.root.clone());
        let in_place = worktree.is_none() && project.work_mode == WorkMode::InPlace;
        let filter = RunFilter {
            status: None,
            thread: Some(job.thread),
        };
        // Resume the agent's latest run here that really started a session
        // with the CLI it uses now. A run that failed before its CLI came up
        // has nothing to resume, and one CLI can't resume another's session.
        // A job's run always starts a session of its own.
        let job_run = db.store.run(job.run)?.job.is_some();
        let earlier = if job_run {
            Vec::new()
        } else {
            db.store.runs(filter)?
        };
        let mut previous = None;
        for run in earlier.into_iter().rev() {
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
            None if job_run => job.prompt.clone(),
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
            && spec.permission_mode.edits()
            && spec.runtime == Runtime::ClaudeCode
        {
            self.add_message(db, job.thread, Author::System, NO_CREW_MCP, Some(job.run))?;
        }
        let wiring = self.0.crew_mcp.as_ref().map(|(bin, home)| McpWiring {
            crew_mcp_bin: bin.clone(),
            run: job.run,
            crew_home: home.clone(),
            report: job_run,
        });
        let context = CrewContext {
            in_place,
            project: project.name.clone(),
            team: db
                .store
                .agents()?
                .into_iter()
                .filter(|(id, _)| *id != job.agent)
                .map(|(_, a)| Teammate {
                    edits: a.permission_mode.edits(),
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
                // Claude Code runs a skill when the prompt starts with it.
                let prompt = match &skill {
                    Some(skill) => format!("/{} {prompt}", skill.name),
                    None => prompt,
                };
                let mut inv = build_claude(
                    &spec,
                    &prompt,
                    &cwd,
                    session,
                    wiring,
                    Some(&context),
                    extra_allowed,
                );
                if let Some(bin) = &self.0.bins.claude {
                    inv.bin = bin.clone();
                }
                if let Some(root) = &read_only_root {
                    inv.add_read_only_dir(root);
                }
                Invocation::Claude(inv)
            }
            Runtime::Codex => {
                // Codex reads anywhere but writes only in its folder, so
                // the original folder needs no grant to be read.
                let original = read_only_root.as_ref().map(|root| {
                    format!(
                        "Your folder holds a worktree of each repo. The original folder {} is there to read; don't change files in it.",
                        root.display()
                    )
                });
                let developer_instructions = [
                    context.render_codex(&spec, wiring.is_some()),
                    original.unwrap_or_default(),
                    spec.instructions.clone(),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
                Invocation::Codex(Box::new(CodexInvocation {
                    bin: crate::runtimes::codex_bin(&self.0.bins),
                    agent: spec.name.clone(),
                    cwd,
                    model: spec.model.clone(),
                    permission_mode: spec.permission_mode.clone(),
                    developer_instructions,
                    prompt: fresh,
                    skill: skill.map(|s| (s.name, s.path)),
                    resume: resume.map(|(thread, prompt)| Resume { thread, prompt }),
                    mcp: wiring,
                    env: Vec::new(),
                }))
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
        // Whether the last turn that counts ended well.
        let mut answered = false;
        let mut model = String::new();
        while let Some(mut event) = rx.recv().await {
            match &event {
                // A resumed session can print an empty result before the
                // one for its prompt. The last result that counts wins.
                Event::TurnEnd { ok, summary: s, .. } if !event.is_empty_turn_end() => {
                    summary = s.clone();
                    answered = *ok;
                }
                Event::Message { text } => texts.push(text.clone()),
                Event::SkillsListed { skills } => {
                    let project = self.db().store.project_of_thread(thread).map(|p| p.id);
                    if let Ok(project) = project {
                        self.0
                            .claude_listed
                            .lock()
                            .unwrap()
                            .insert(project, skills.iter().cloned().collect());
                    }
                }
                Event::SessionStarted {
                    session_id,
                    model: started,
                    ..
                } => {
                    model.clone_from(started);
                    self.name_session(run, session_id);
                }
                _ => {}
            }
            if let Event::Usage { context_window, .. } = &mut event {
                self.remember_window(&model, context_window);
            }
            let denied = match &event {
                Event::PermissionDenied {
                    tool,
                    input,
                    reason,
                } => Some((tool.clone(), input.clone(), reason.clone())),
                _ => None,
            };
            self.record_event(thread, run, event);
            // Only a refusal by the safety check, with its reason, is put to
            // you. Any other denial is just a call that wasn't allowed.
            if let Some((tool, input, Some(reason))) = denied {
                self.note_blocked(run, &tool, input, reason);
            }
        }
        let outcome = join
            .await
            .unwrap_or_else(|e| RunOutcome::Failed(format!("The run task ended: {e}")));
        // In place: what the run left, so its changes can be undone.
        self.record_end(thread, run).await;
        let reply = if !answered && outcome != RunOutcome::Done {
            // A run stopped before it answered has nothing to post.
            String::new()
        } else if summary.is_empty() {
            texts.join("\n")
        } else {
            summary
        };
        if let Err(e) = self.finish(thread, run, agent, &name, outcome, reply) {
            log(format!("couldn't record the end of run {run}: {e}"));
        }
        self.start_ready();
    }

    /// Keeps the context window `model` reported, or fills it in from the
    /// last one kept when the event has none, as Claude's usage lines don't.
    fn remember_window(&self, model: &str, window: &mut Option<u64>) {
        if model.is_empty() {
            return;
        }
        let db = self.db();
        match *window {
            Some(tokens) => {
                if db.store.context_window(model).ok().flatten() == Some(tokens) {
                    return;
                }
                if let Err(e) = db.store.set_context_window(model, tokens) {
                    log(format!("couldn't save the context window of {model}: {e}"));
                }
            }
            None => *window = db.store.context_window(model).ok().flatten(),
        }
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

    /// Records how a run ended and posts what it has to say. `reply` is
    /// its answer, empty if it gave none.
    fn finish(
        &self,
        thread: ThreadId,
        run: RunId,
        agent: AgentId,
        name: &str,
        outcome: RunOutcome,
        reply: String,
    ) -> Result<(), StoreError> {
        // The run may have changed files.
        self.changes_may_differ(thread);
        let mut state = self.state();
        // The process is gone, so dropping the handle does nothing more.
        Self::end_live(&mut state, run);
        // A run that ended in its worktree setup never used its rule.
        state.retry_rules.remove(&run);
        let limit = state.timed_out.remove(&run);
        let handed = state.handed.remove(&run).unwrap_or_default();
        state.scheduler.finished(thread);
        let mut db = self.db();
        self.cancel_permissions(&mut state, &mut db, run);
        self.expire_blocked_after(&mut db, thread, agent, run);
        let done = matches!(outcome, RunOutcome::Done);
        // A run stopped after it answered still posts its answer, ahead of
        // the note that it stopped.
        if matches!(outcome, RunOutcome::Stopped) && !state.shutting_down && !reply.is_empty() {
            self.add_message(&mut db, thread, Author::Agent(agent), &reply, Some(run))?;
        }
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
            let id = db.store.add_message(thread, author, &body, Some(run))?;
            // Quiet before anyone hears of it, so it never notifies.
            if crate::jobs::quiet_reply(&db, run, &status) {
                db.store.set_message_quiet(id)?;
            }
            self.publish_message(&mut db, thread, id)?;
        }
        self.publish_status(&mut db, thread, run, status);
        self.job_run_ended(&mut db, run);
        if done && !state.shutting_down {
            self.reply_handoffs(&mut state, &mut db, run, agent, &reply, &handed)?;
        }
        Ok(())
    }

    /// Saves a run's event and notifies the thread.
    fn record_event(&self, thread: ThreadId, run: RunId, event: Event) {
        self.note_edit(thread, &event);
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
                details: Box::new(details),
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

    pub(crate) fn publish_message(
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
    Codex(Box<CodexInvocation>),
}

/// A folder as compared between projects: links resolved when it exists,
/// trailing slashes dropped.
pub(crate) fn same_folder(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.components().collect())
}

/// What a new project is made with.
struct NewProject<'a> {
    name: &'a str,
    root: &'a Path,
    main: &'a str,
    work_mode: Option<WorkMode>,
    copy_files: &'a [String],
    setup_command: Option<&'a str>,
}

/// Runs blocking work, such as git, off the async threads.
pub(crate) async fn blocking<T, E>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, String>
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

/// A model override with blanks trimmed, none for an empty one. It goes
/// to the CLI as an argument: anything that could pass for a flag or break
/// the value is refused.
pub(crate) fn tidy_model_override(model: Option<String>) -> Result<Option<String>, String> {
    let model = model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    if let Some(m) = &model
        && (m.starts_with('-')
            || m.chars().any(|c| c.is_whitespace() || c.is_control())
            || m.chars().count() > 128)
    {
        return Err("That model name isn't valid. Pick one from the list.".into());
    }
    Ok(model)
}

/// Now, in Unix ms.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
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
            waiting_for: None,
            skill: None,
            model_override: None,
            message: None,
            job: None,
            found: None,
            summary: None,
            missed: 0,
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

    fn git_repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ][..],
        ] {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }
    }

    #[tokio::test]
    async fn a_retry_that_never_launches_forgets_its_rule() {
        use super::{CliBins, Service};
        use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
        use crew_core::scheduler::Job;
        use crew_core::store::Store;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        git_repo(&root);
        let svc = Service::new(
            Store::open_in_memory().unwrap(),
            2,
            CliBins {
                claude: Some("/usr/bin/false".into()),
                ..Default::default()
            },
            tmp.path().join("worktrees"),
            None,
            Duration::from_secs(60),
            64,
        );
        let (thread, agent) = {
            let db = svc.db();
            let p = db.store.create_project("demo", &root, "main").unwrap();
            // The worktree's setup fails, so the run never starts.
            db.store.set_project_setup(p, Some("exit 1"), &[]).unwrap();
            let c = db.store.create_channel(p, "general").unwrap();
            let t = db.store.create_thread(c, "work").unwrap();
            let a = db
                .store
                .create_agent(&AgentSpec {
                    name: "Forge".into(),
                    role: String::new(),
                    runtime: Runtime::ClaudeCode,
                    model: "sonnet".into(),
                    instructions: String::new(),
                    permission_mode: PermissionMode::Auto,
                    allowed_tools: vec![],
                    handoff_policy: None,
                    handoff_budget: None,
                    max_run_minutes: 0,
                })
                .unwrap();
            (t, a)
        };
        let queue = |svc: &Service| {
            let run = svc.db().store.create_run(thread, agent).unwrap();
            let mut state = svc.state();
            state.scheduler.enqueue(Job {
                run,
                thread,
                agent,
                prompt: "go".into(),
            });
            state.retry_rules.insert(run, vec!["Bash(ls)".into()]);
            run
        };

        // Stopped while still queued.
        let stopped = queue(&svc);
        svc.stop_run(stopped).unwrap();
        assert!(svc.state().retry_rules.is_empty());

        // Its worktree setup failed.
        let failed = queue(&svc);
        svc.start_ready();
        let started = Instant::now();
        while svc.db().store.run(failed).unwrap().status != RunStatus::Failed {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the run never failed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(svc.state().retry_rules.is_empty());
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
