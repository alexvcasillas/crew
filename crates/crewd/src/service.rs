//! Chat state and run handling behind the server.
//!
//! Lock order: `state` before `db`. Neither lock is held across an `.await`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crew_core::agent::AgentSpec;
use crew_core::claude::{ClaudeInvocation, SessionArg, build_claude};
use crew_core::event::{Event, RunStatus};
use crew_core::ids::{AgentId, MessageId, RunId, ThreadId};
use crew_core::mention::parse_mentions;
use crew_core::scheduler::{Job, Scheduler};
use crew_core::store::{Author, RunFilter, Store, StoreError};
use crew_proto::{Request, ServerMsg};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::runner::{RunHandle, RunOutcome, spawn_run};

/// Capacity of the notification channel.
const NOTICE_BUFFER: usize = 4096;
/// Name used for the user's messages in a new session's context.
const YOU_NAME: &str = "You";

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

struct State {
    scheduler: Scheduler,
    live: HashMap<RunId, (ThreadId, RunHandle)>,
    /// Prompts for queued jobs in case they start a new session, in the
    /// same order the scheduler hands out jobs of each thread and agent.
    fresh_prompts: HashMap<(ThreadId, AgentId), VecDeque<String>>,
}

struct Inner {
    db: Mutex<Db>,
    state: Mutex<State>,
    tx: broadcast::Sender<Notice>,
    claude_bin: Option<PathBuf>,
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
    pub fn new(store: Store, max_runs: usize, claude_bin: Option<PathBuf>) -> Service {
        let (tx, _) = broadcast::channel(NOTICE_BUFFER);
        Service(Arc::new(Inner {
            db: Mutex::new(Db { store, serial: 0 }),
            state: Mutex::new(State {
                scheduler: Scheduler::new(max_runs),
                live: HashMap::new(),
                fresh_prompts: HashMap::new(),
            }),
            tx,
            claude_bin,
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
    pub fn handle(&self, req: Request) -> Reply {
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
        }
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
                state.scheduler.enqueue(Job {
                    thread,
                    agent: *agent,
                    prompt: body.to_string(),
                });
                state
                    .fresh_prompts
                    .entry((thread, *agent))
                    .or_default()
                    .push_back(fresh.clone());
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

    fn stop_run(&self, run: RunId) -> Result<(), String> {
        match self.state().live.get(&run) {
            Some((_, handle)) => {
                handle.stop();
                Ok(())
            }
            None => Err(format!("Run {run} isn't running.")),
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
        state.fresh_prompts.retain(|(t, _), _| *t != thread);
        self.note_dropped(&dropped)
    }

    /// Stops every live run and drops the whole queue.
    fn stop_all(&self) -> Result<(), StoreError> {
        let mut state = self.state();
        for (_, handle) in state.live.values() {
            handle.stop();
        }
        let dropped = state.scheduler.clear();
        state.fresh_prompts.clear();
        self.note_dropped(&dropped)
    }

    /// Tells each thread that lost queued jobs. Queued jobs have no run yet.
    fn note_dropped(&self, dropped: &[Job]) -> Result<(), StoreError> {
        let mut threads: Vec<ThreadId> = Vec::new();
        for job in dropped {
            if !threads.contains(&job.thread) {
                threads.push(job.thread);
            }
        }
        let mut db = self.db();
        for thread in threads {
            self.add_message(&mut db, thread, Author::System, "Stopped before starting.")?;
        }
        Ok(())
    }

    /// Starts queued jobs while the scheduler allows.
    fn start_ready(&self) {
        let mut state = self.state();
        while let Some(job) = state.scheduler.next_ready() {
            let key = (job.thread, job.agent);
            let fresh = state
                .fresh_prompts
                .get_mut(&key)
                .and_then(|q| q.pop_front());
            if state.fresh_prompts.get(&key).is_some_and(|q| q.is_empty()) {
                state.fresh_prompts.remove(&key);
            }
            let mut db = self.db();
            match self.prepare(&mut db, &job, fresh) {
                Ok((run, name, inv)) => {
                    drop(db);
                    let (handle, rx, join) = spawn_run(inv);
                    state.live.insert(run, (job.thread, handle));
                    let svc = self.clone();
                    tokio::spawn(svc.watch(job.thread, run, job.agent, name, rx, join));
                }
                Err(e) => {
                    log(format!(
                        "couldn't start a run in thread {}: {e}",
                        job.thread
                    ));
                    state.scheduler.finished(job.thread);
                    let body = format!("Couldn't start the run: {e}");
                    if let Err(e) = self.add_message(&mut db, job.thread, Author::System, &body) {
                        log(e);
                    }
                }
            }
        }
    }

    /// Creates the run row and builds the command line for a job.
    fn prepare(
        &self,
        db: &mut Db,
        job: &Job,
        fresh: Option<String>,
    ) -> Result<(RunId, String, ClaudeInvocation), StoreError> {
        let spec = db
            .store
            .agents()?
            .into_iter()
            .find(|(id, _)| *id == job.agent)
            .map(|(_, spec)| spec)
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
            Some(run) => (
                run.session_id.clone(),
                SessionArg::Resume(run.session_id),
                job.prompt.clone(),
            ),
            None => {
                let id = Uuid::new_v4();
                let prompt = fresh.unwrap_or_else(|| job.prompt.clone());
                (id.to_string(), SessionArg::New(id), prompt)
            }
        };
        let run = db.store.create_run(job.thread, job.agent, &session_id)?;
        self.publish(
            db,
            job.thread,
            ServerMsg::RunStatus {
                thread: job.thread,
                run,
                status: RunStatus::Working,
            },
        );
        let mut inv = build_claude(&spec, &prompt, &cwd, session);
        if let Some(bin) = &self.0.claude_bin {
            inv.bin = bin.clone();
        }
        Ok((run, spec.name, inv))
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
        self.publish(
            &mut db,
            thread,
            ServerMsg::RunStatus {
                thread,
                run,
                status,
            },
        );
        Ok(())
    }

    /// Saves a message and notifies the thread.
    fn add_message(
        &self,
        db: &mut Db,
        thread: ThreadId,
        author: Author,
        body: &str,
    ) -> Result<MessageId, StoreError> {
        let id = db.store.add_message(thread, author, body)?;
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

/// Earlier messages as plain text, one per line.
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
