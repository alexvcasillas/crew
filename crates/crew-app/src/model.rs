//! What the app knows about crewd, built only from what crewd sends. Pure:
//! no IO and no UI types, so it is tested on its own.

use std::collections::{HashMap, HashSet};

use crew_core::event::{Event, RunStatus};
use crew_core::ids::{AgentId, ApprovalId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::store::{
    ApprovalStatus, Author, StoredAgent, StoredApproval, StoredChannel, StoredMessage,
    StoredProject, StoredRun, StoredThread,
};
use crew_proto::ServerMsg;

/// Whether the app can talk to crewd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    /// The connection dropped and the client is connecting again. What is
    /// known stays shown meanwhile.
    Reconnecting,
    /// Nothing listens on crewd's socket.
    NotRunning,
    /// crewd answered but refused this app's protocol version.
    VersionMismatch {
        theirs: u32,
        ours: u32,
    },
}

/// What a thread, or a project's threads, need from you.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThreadStatus {
    Idle,
    Working,
    NeedsYou,
}

/// A pending approval: something waiting for you to decide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NeedsYou<'a> {
    pub approval: &'a StoredApproval,
}

/// One event of a run, in a watched thread.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedEvent {
    pub run: RunId,
    pub seq: u64,
    pub event: Event,
}

/// The events of one watched thread, each `(run, seq)` once.
#[derive(Debug, Default)]
struct EventLog {
    events: Vec<LoggedEvent>,
    seen: HashSet<(RunId, u64)>,
}

/// Everything the UI renders from.
#[derive(Debug, Default)]
pub struct State {
    /// `None` until the client first reports.
    connection: Option<ConnectionState>,
    projects: HashMap<ProjectId, StoredProject>,
    channels: HashMap<ChannelId, StoredChannel>,
    threads: HashMap<ThreadId, StoredThread>,
    agents: HashMap<AgentId, StoredAgent>,
    runs: HashMap<RunId, StoredRun>,
    /// Sorted by `created_at`, ties in arrival order. Complete only for
    /// watched threads: other threads have what arrived since connecting.
    messages: HashMap<ThreadId, Vec<StoredMessage>>,
    /// Pending ones and those decided since connecting.
    approvals: HashMap<ApprovalId, StoredApproval>,
    /// One log per watched thread.
    logs: HashMap<ThreadId, EventLog>,
}

impl State {
    pub fn connection(&self) -> Option<&ConnectionState> {
        self.connection.as_ref()
    }

    pub fn set_connection(&mut self, state: ConnectionState) {
        self.connection = Some(state);
    }

    /// Takes in one message from crewd. Copies of what is already known,
    /// as crewd may resend after a subscribe, change nothing.
    pub fn apply(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Snapshot {
                projects,
                channels,
                threads,
                agents,
                runs,
                approvals,
            } => self.reset_from_snapshot(projects, channels, threads, agents, runs, approvals),
            ServerMsg::ProjectChanged { project } => {
                self.projects.insert(project.id, project);
            }
            ServerMsg::ChannelChanged { channel } => {
                self.channels.insert(channel.id, channel);
            }
            ServerMsg::ThreadCreated { thread, .. } | ServerMsg::ThreadChanged { thread } => {
                self.threads.insert(thread.id, thread);
            }
            ServerMsg::AgentChanged { agent } => {
                self.agents.insert(agent.id, agent);
            }
            ServerMsg::RunStatus { details, .. } => self.run_changed(details),
            ServerMsg::Message { thread, message } => self.add_message(thread, message),
            ServerMsg::Approval { approval, .. } => self.add_approval(approval),
            ServerMsg::Event {
                thread,
                run,
                seq,
                event,
            } => {
                if let Some(log) = self.logs.get_mut(&thread)
                    && log.seen.insert((run, seq))
                {
                    log.events.push(LoggedEvent { run, seq, event });
                }
            }
            ServerMsg::Response { .. } | ServerMsg::Resync => {}
        }
    }

    /// Replaces everything with what a fresh `SubscribeAll` starts from.
    /// Watched threads stay watched with empty logs, for their backlog to
    /// fill again.
    fn reset_from_snapshot(
        &mut self,
        projects: Vec<StoredProject>,
        channels: Vec<StoredChannel>,
        threads: Vec<StoredThread>,
        agents: Vec<StoredAgent>,
        runs: Vec<StoredRun>,
        approvals: Vec<StoredApproval>,
    ) {
        self.projects = projects.into_iter().map(|p| (p.id, p)).collect();
        self.channels = channels.into_iter().map(|c| (c.id, c)).collect();
        self.threads = threads.into_iter().map(|t| (t.id, t)).collect();
        self.agents = agents.into_iter().map(|a| (a.id, a)).collect();
        self.runs = runs.into_iter().map(|r| (r.id, r)).collect();
        self.messages.clear();
        self.approvals = approvals.into_iter().map(|a| (a.id, a)).collect();
        for log in self.logs.values_mut() {
            *log = EventLog::default();
        }
    }

    /// Keeps the run as sent, unless a further-along copy is known.
    fn run_changed(&mut self, run: StoredRun) {
        let stale = self
            .runs
            .get(&run.id)
            .is_some_and(|known| stage(&run.status) < stage(&known.status));
        if !stale {
            self.runs.insert(run.id, run);
        }
    }

    fn add_message(&mut self, thread: ThreadId, message: StoredMessage) {
        let messages = self.messages.entry(thread).or_default();
        if let Some(known) = messages.iter_mut().find(|m| m.id == message.id) {
            *known = message;
            return;
        }
        let at = messages.partition_point(|m| m.created_at <= message.created_at);
        messages.insert(at, message);
    }

    fn add_approval(&mut self, approval: StoredApproval) {
        let stale = self.approvals.get(&approval.id).is_some_and(|known| {
            known.status != ApprovalStatus::Pending && approval.status == ApprovalStatus::Pending
        });
        if !stale {
            self.approvals.insert(approval.id, approval);
        }
    }

    /// Keeps `thread`'s events from now on. crewd's backlog for it fills
    /// the log.
    pub fn watch(&mut self, thread: ThreadId) {
        self.logs.entry(thread).or_default();
    }

    /// Drops `thread`'s events and stops keeping them.
    pub fn unwatch(&mut self, thread: ThreadId) {
        self.logs.remove(&thread);
    }

    pub fn is_watched(&self, thread: ThreadId) -> bool {
        self.logs.contains_key(&thread)
    }

    /// Projects, oldest first.
    pub fn projects(&self) -> Vec<&StoredProject> {
        sorted(self.projects.values(), |p| (p.created_at, p.id.0))
    }

    /// The project's channels, oldest first.
    pub fn channels(&self, project: ProjectId) -> Vec<&StoredChannel> {
        sorted(
            self.channels.values().filter(|c| c.project == project),
            |c| (c.created_at, c.id.0),
        )
    }

    /// The channel's threads, oldest first.
    pub fn threads(&self, channel: ChannelId) -> Vec<&StoredThread> {
        sorted(
            self.threads.values().filter(|t| t.channel == channel),
            |t| (t.created_at, t.id.0),
        )
    }

    pub fn run(&self, run: RunId) -> Option<&StoredRun> {
        self.runs.get(&run)
    }

    pub fn project(&self, project: ProjectId) -> Option<&StoredProject> {
        self.projects.get(&project)
    }

    pub fn channel(&self, channel: ChannelId) -> Option<&StoredChannel> {
        self.channels.get(&channel)
    }

    pub fn thread(&self, thread: ThreadId) -> Option<&StoredThread> {
        self.threads.get(&thread)
    }

    /// Any agent, deleted ones included, so old messages can name them.
    pub fn agent(&self, agent: AgentId) -> Option<&StoredAgent> {
        self.agents.get(&agent)
    }

    /// Agents that aren't deleted, oldest first.
    pub fn agents(&self) -> Vec<&StoredAgent> {
        sorted(
            self.agents.values().filter(|a| a.deleted_at.is_none()),
            |a| (a.created_at, a.id.0),
        )
    }

    /// The thread's messages, oldest first.
    pub fn messages(&self, thread: ThreadId) -> &[StoredMessage] {
        self.messages.get(&thread).map_or(&[], Vec::as_slice)
    }

    /// The watched thread's events in arrival order. Empty if it isn't
    /// watched.
    pub fn events(&self, thread: ThreadId) -> &[LoggedEvent] {
        self.logs
            .get(&thread)
            .map_or(&[], |log| log.events.as_slice())
    }

    /// Pending approvals, oldest first.
    pub fn needs_you(&self) -> Vec<NeedsYou<'_>> {
        sorted(
            self.approvals
                .values()
                .filter(|a| a.status == ApprovalStatus::Pending),
            |a| (a.created_at, a.id.0),
        )
        .into_iter()
        .map(|approval| NeedsYou { approval })
        .collect()
    }

    /// Queued and working runs, by start time.
    pub fn running(&self) -> Vec<&StoredRun> {
        sorted(self.runs.values().filter(|r| is_live(&r.status)), |r| {
            (r.started_at, r.id.0)
        })
    }

    pub fn thread_status(&self, thread: ThreadId) -> ThreadStatus {
        self.status_of(|t| t == thread)
    }

    /// The most urgent status among the project's threads.
    pub fn project_status(&self, project: ProjectId) -> ThreadStatus {
        let channels: HashSet<ChannelId> = self
            .channels
            .values()
            .filter(|c| c.project == project)
            .map(|c| c.id)
            .collect();
        let threads: HashSet<ThreadId> = self
            .threads
            .values()
            .filter(|t| channels.contains(&t.channel))
            .map(|t| t.id)
            .collect();
        self.status_of(|t| threads.contains(&t))
    }

    /// The most urgent status among the channel's threads.
    pub fn channel_status(&self, channel: ChannelId) -> ThreadStatus {
        let threads = self.thread_ids(|c| c == channel);
        self.status_of(|t| threads.contains(&t))
    }

    /// Whether the agent waits on you, works, or neither.
    pub fn agent_status(&self, agent: AgentId) -> ThreadStatus {
        let asks = self.approvals.values().any(|a| {
            a.status == ApprovalStatus::Pending
                && self.runs.get(&a.run).is_some_and(|r| r.agent == agent)
        });
        if asks {
            return ThreadStatus::NeedsYou;
        }
        let working = self
            .runs
            .values()
            .any(|r| r.agent == agent && is_live(&r.status));
        if working {
            ThreadStatus::Working
        } else {
            ThreadStatus::Idle
        }
    }

    /// Messages in the channel's threads after `since` (Unix ms) that you
    /// didn't write.
    pub fn unread(&self, channel: ChannelId, since: i64) -> usize {
        let threads = self.thread_ids(|c| c == channel);
        threads
            .iter()
            .flat_map(|t| self.messages(*t))
            .filter(|m| m.created_at > since && m.author != Author::You)
            .count()
    }

    /// Agents with a working run, each once. Queued runs don't count.
    pub fn working_agents(&self) -> Vec<AgentId> {
        let mut agents: Vec<AgentId> = Vec::new();
        for run in self.running() {
            if run.status == RunStatus::Working && !agents.contains(&run.agent) {
                agents.push(run.agent);
            }
        }
        agents
    }

    /// The thread's queued and working runs, by start time.
    pub fn live_runs(&self, thread: ThreadId) -> Vec<&StoredRun> {
        sorted(
            self.runs
                .values()
                .filter(|r| r.thread == thread && is_live(&r.status)),
            |r| (r.started_at, r.id.0),
        )
    }

    /// The thread's pending approvals, oldest first.
    pub fn pending_approvals(&self, thread: ThreadId) -> Vec<&StoredApproval> {
        sorted(
            self.approvals
                .values()
                .filter(|a| a.thread == thread && a.status == ApprovalStatus::Pending),
            |a| (a.created_at, a.id.0),
        )
    }

    /// Every known run of the thread, by start time. Watching the thread
    /// brings its past runs.
    pub fn thread_runs(&self, thread: ThreadId) -> Vec<&StoredRun> {
        sorted(self.runs.values().filter(|r| r.thread == thread), |r| {
            (r.started_at, r.id.0)
        })
    }

    pub fn approval(&self, approval: ApprovalId) -> Option<&StoredApproval> {
        self.approvals.get(&approval)
    }

    /// The thread's known approvals, pending and decided, oldest first.
    pub fn thread_approvals(&self, thread: ThreadId) -> Vec<&StoredApproval> {
        sorted(
            self.approvals.values().filter(|a| a.thread == thread),
            |a| (a.created_at, a.id.0),
        )
    }

    /// The thread's newest known message.
    pub fn last_message(&self, thread: ThreadId) -> Option<&StoredMessage> {
        self.messages(thread).last()
    }

    /// Threads whose channel `matches` accepts.
    fn thread_ids(&self, matches: impl Fn(ChannelId) -> bool) -> HashSet<ThreadId> {
        self.threads
            .values()
            .filter(|t| matches(t.channel))
            .map(|t| t.id)
            .collect()
    }

    /// The most urgent status among threads `matches` accepts.
    fn status_of(&self, matches: impl Fn(ThreadId) -> bool) -> ThreadStatus {
        let asks = self
            .approvals
            .values()
            .any(|a| a.status == ApprovalStatus::Pending && matches(a.thread));
        if asks {
            return ThreadStatus::NeedsYou;
        }
        let working = self
            .runs
            .values()
            .any(|r| is_live(&r.status) && matches(r.thread));
        if working {
            ThreadStatus::Working
        } else {
            ThreadStatus::Idle
        }
    }
}

/// How far along a run is. Statuses never move back a stage.
fn stage(status: &RunStatus) -> u8 {
    match status {
        RunStatus::Queued => 0,
        RunStatus::Working => 1,
        RunStatus::Done | RunStatus::Failed | RunStatus::Stopped | RunStatus::Interrupted => 2,
    }
}

fn is_live(status: &RunStatus) -> bool {
    matches!(status, RunStatus::Queued | RunStatus::Working)
}

/// `items` sorted by `key`, which must be unique so the order is stable.
fn sorted<'a, T, K: Ord>(items: impl Iterator<Item = &'a T>, key: impl Fn(&T) -> K) -> Vec<&'a T> {
    let mut items: Vec<&T> = items.collect();
    items.sort_by_key(|item| key(item));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
    use crew_core::store::ApprovalKind;
    use std::path::PathBuf;

    fn project(created_at: i64) -> StoredProject {
        StoredProject {
            id: ProjectId::new(),
            name: "demo".into(),
            root: PathBuf::from("/work/demo"),
            main_branch: "main".into(),
            created_at,
            setup_command: None,
            copy_files: vec![],
        }
    }

    fn channel(project: ProjectId) -> StoredChannel {
        StoredChannel {
            id: ChannelId::new(),
            project,
            name: "general".into(),
            created_at: 1,
            handoff_policy: None,
            handoff_budget: None,
        }
    }

    fn thread(channel: ChannelId, title: &str, created_at: i64) -> StoredThread {
        StoredThread {
            id: ThreadId::new(),
            channel,
            title: title.into(),
            created_at,
            worktree: None,
            handoff_policy: None,
            handoff_budget: None,
            handoffs_used: 0,
        }
    }

    fn agent(name: &str) -> StoredAgent {
        StoredAgent {
            id: AgentId::new(),
            spec: AgentSpec {
                name: name.into(),
                role: String::new(),
                runtime: Runtime::ClaudeCode,
                model: "claude-haiku-4-5".into(),
                instructions: String::new(),
                permission_mode: PermissionMode::ReadOnly,
                allowed_tools: vec![],
                handoff_policy: None,
                handoff_budget: None,
                max_run_minutes: 20,
            },
            created_at: 1,
            deleted_at: None,
        }
    }

    fn run(thread: ThreadId, status: RunStatus, started_at: i64) -> StoredRun {
        StoredRun {
            id: RunId::new(),
            thread,
            agent: AgentId::new(),
            session_id: String::new(),
            status,
            started_at,
            ended_at: None,
        }
    }

    fn message(thread: ThreadId, body: &str, created_at: i64) -> StoredMessage {
        StoredMessage {
            id: crew_core::ids::MessageId::new(),
            thread,
            author: Author::You,
            body: body.into(),
            created_at,
            run: None,
            approval: None,
        }
    }

    fn approval(thread: ThreadId, created_at: i64) -> StoredApproval {
        StoredApproval {
            id: ApprovalId::new(),
            thread,
            run: RunId::new(),
            kind: ApprovalKind::Permission {
                tool: "Bash".into(),
                input: serde_json::json!({ "command": "touch x.txt" }),
                tool_use_id: None,
            },
            status: ApprovalStatus::Pending,
            created_at,
            decided_at: None,
        }
    }

    fn event(thread: ThreadId, run: RunId, seq: u64, text: &str) -> ServerMsg {
        ServerMsg::Event {
            thread,
            run,
            seq,
            event: Event::Message { text: text.into() },
        }
    }

    fn status(run: &StoredRun) -> ServerMsg {
        ServerMsg::RunStatus {
            thread: run.thread,
            run: run.id,
            status: run.status.clone(),
            details: run.clone(),
        }
    }

    /// `run` after it moved to `status`. A terminal status ends it at 99.
    fn moved(run: &StoredRun, status: RunStatus) -> StoredRun {
        let ended = !matches!(status, RunStatus::Queued | RunStatus::Working);
        StoredRun {
            status,
            ended_at: ended.then_some(99),
            ..run.clone()
        }
    }

    /// A state holding one project, channel and thread.
    fn one_thread() -> (State, StoredProject, StoredChannel, StoredThread) {
        let p = project(1);
        let c = channel(p.id);
        let t = thread(c.id, "fix the login", 1);
        let mut state = State::default();
        state.apply(ServerMsg::Snapshot {
            projects: vec![p.clone()],
            channels: vec![c.clone()],
            threads: vec![t.clone()],
            agents: vec![],
            runs: vec![],
            approvals: vec![],
        });
        (state, p, c, t)
    }

    fn texts(state: &State, thread: ThreadId) -> Vec<String> {
        state
            .events(thread)
            .iter()
            .map(|e| match &e.event {
                Event::Message { text } => text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn events_are_kept_once_per_run_and_seq() {
        let (mut state, _, _, t) = one_thread();
        let r = RunId::new();
        state.watch(t.id);
        state.apply(event(t.id, r, 1, "a"));
        state.apply(event(t.id, r, 2, "b"));
        state.apply(event(t.id, r, 1, "a"));
        state.apply(event(t.id, r, 2, "b"));
        let other = RunId::new();
        state.apply(event(t.id, other, 1, "c"));
        assert_eq!(texts(&state, t.id), ["a", "b", "c"]);
    }

    #[test]
    fn events_of_unwatched_threads_are_dropped() {
        let (mut state, _, _, t) = one_thread();
        let r = RunId::new();
        state.apply(event(t.id, r, 1, "before"));
        assert!(state.events(t.id).is_empty());
        state.watch(t.id);
        state.apply(event(t.id, r, 2, "during"));
        state.unwatch(t.id);
        state.apply(event(t.id, r, 3, "after"));
        assert!(state.events(t.id).is_empty());
        // Watching again starts from the backlog crewd resends.
        state.watch(t.id);
        state.apply(event(t.id, r, 1, "before"));
        state.apply(event(t.id, r, 2, "during"));
        state.apply(event(t.id, r, 3, "after"));
        assert_eq!(texts(&state, t.id), ["before", "during", "after"]);
    }

    #[test]
    fn run_status_moves_forward_only() {
        let (mut state, _, _, t) = one_thread();
        let queued = run(t.id, RunStatus::Queued, 5);
        state.apply(status(&queued));
        assert_eq!(state.thread_status(t.id), ThreadStatus::Working);
        let working = moved(&queued, RunStatus::Working);
        state.apply(status(&working));
        assert_eq!(state.running(), [&working]);
        let done = moved(&working, RunStatus::Done);
        state.apply(status(&done));
        assert!(state.running().is_empty());
        assert_eq!(state.thread_status(t.id), ThreadStatus::Idle);
        // A late copy of an older status, as after a resubscribe, is ignored.
        state.apply(status(&working));
        assert!(state.running().is_empty());
        assert_eq!(state.thread_status(t.id), ThreadStatus::Idle);
    }

    #[test]
    fn a_run_status_brings_the_whole_run() {
        let (mut state, _, _, t) = one_thread();
        let queued = run(t.id, RunStatus::Queued, 5);
        state.apply(status(&queued));
        assert_eq!(state.running(), [&queued], "known from its first status");
        let done = moved(&queued, RunStatus::Done);
        state.apply(status(&done));
        assert_eq!(state.run(queued.id), Some(&done));
        assert_eq!(state.run(queued.id).unwrap().ended_at, Some(99));
    }

    #[test]
    fn agent_changes_bring_the_agent() {
        let (mut state, _, _, _) = one_thread();
        let forge = agent("Forge");
        state.apply(ServerMsg::AgentChanged {
            agent: forge.clone(),
        });
        assert_eq!(state.agents(), [&forge]);
        let mut deleted = forge.clone();
        deleted.deleted_at = Some(9);
        state.apply(ServerMsg::AgentChanged {
            agent: deleted.clone(),
        });
        assert!(state.agents().is_empty());
        assert_eq!(
            state.agent(forge.id),
            Some(&deleted),
            "still names old messages"
        );
    }

    #[test]
    fn approvals_go_from_pending_to_decided() {
        let (mut state, _, _, t) = one_thread();
        let first = approval(t.id, 10);
        let second = approval(t.id, 5);
        for a in [&first, &second] {
            state.apply(ServerMsg::Approval {
                thread: t.id,
                approval: a.clone(),
            });
        }
        let ids: Vec<ApprovalId> = state.needs_you().iter().map(|n| n.approval.id).collect();
        assert_eq!(ids, [second.id, first.id], "oldest first");
        assert_eq!(state.thread_status(t.id), ThreadStatus::NeedsYou);

        let mut decided = first.clone();
        decided.status = ApprovalStatus::Approved;
        decided.decided_at = Some(20);
        state.apply(ServerMsg::Approval {
            thread: t.id,
            approval: decided.clone(),
        });
        // A late pending copy doesn't bring it back.
        state.apply(ServerMsg::Approval {
            thread: t.id,
            approval: first.clone(),
        });
        let ids: Vec<ApprovalId> = state.needs_you().iter().map(|n| n.approval.id).collect();
        assert_eq!(ids, [second.id]);

        let mut cancelled = second.clone();
        cancelled.status = ApprovalStatus::Cancelled;
        state.apply(ServerMsg::Approval {
            thread: t.id,
            approval: cancelled,
        });
        assert!(state.needs_you().is_empty());
        assert_eq!(state.thread_status(t.id), ThreadStatus::Idle);
    }

    #[test]
    fn messages_are_ordered_by_time_and_kept_once() {
        let (mut state, _, _, t) = one_thread();
        let late = message(t.id, "late", 30);
        let early = message(t.id, "early", 10);
        let tie_a = message(t.id, "tie a", 20);
        let tie_b = message(t.id, "tie b", 20);
        for m in [&late, &early, &tie_a, &tie_b, &late, &early] {
            state.apply(ServerMsg::Message {
                thread: t.id,
                message: m.clone(),
            });
        }
        let bodies: Vec<&str> = state
            .messages(t.id)
            .iter()
            .map(|m| m.body.as_str())
            .collect();
        assert_eq!(bodies, ["early", "tie a", "tie b", "late"]);
    }

    #[test]
    fn project_status_is_its_most_urgent_thread() {
        let (mut state, p, c, t) = one_thread();
        let other_project = project(2);
        let other_channel = channel(other_project.id);
        let busy = thread(c.id, "busy", 2);
        let elsewhere = thread(other_channel.id, "elsewhere", 3);
        state.apply(ServerMsg::ProjectChanged {
            project: other_project.clone(),
        });
        state.apply(ServerMsg::ChannelChanged {
            channel: other_channel.clone(),
        });
        for th in [&busy, &elsewhere] {
            state.apply(ServerMsg::ThreadCreated {
                channel: th.channel,
                thread: th.clone(),
            });
        }
        assert_eq!(state.project_status(p.id), ThreadStatus::Idle);
        state.apply(status(&run(busy.id, RunStatus::Working, 1)));
        assert_eq!(state.project_status(p.id), ThreadStatus::Working);
        state.apply(ServerMsg::Approval {
            thread: t.id,
            approval: approval(t.id, 1),
        });
        assert_eq!(state.project_status(p.id), ThreadStatus::NeedsYou);
        assert_eq!(state.project_status(other_project.id), ThreadStatus::Idle);
    }

    #[test]
    fn running_is_sorted_by_start_time() {
        let (mut state, _, _, t) = one_thread();
        let b = run(t.id, RunStatus::Working, 20);
        let a = run(t.id, RunStatus::Queued, 10);
        let done = run(t.id, RunStatus::Done, 5);
        for r in [&b, &done, &a] {
            state.apply(status(r));
        }
        let ids: Vec<RunId> = state.running().iter().map(|r| r.id).collect();
        assert_eq!(ids, [a.id, b.id]);
    }

    #[test]
    fn threads_are_sorted_and_follow_renames() {
        let (mut state, _, c, t) = one_thread();
        let newer = thread(c.id, "newer", 5);
        state.apply(ServerMsg::ThreadCreated {
            channel: c.id,
            thread: newer.clone(),
        });
        let mut renamed = t.clone();
        renamed.title = "fix the login page".into();
        state.apply(ServerMsg::ThreadChanged { thread: renamed });
        let titles: Vec<&str> = state
            .threads(c.id)
            .iter()
            .map(|t| t.title.as_str())
            .collect();
        assert_eq!(titles, ["fix the login page", "newer"]);
    }

    #[test]
    fn a_snapshot_replaces_everything_and_empties_watched_logs() {
        let (mut state, _, c, t) = one_thread();
        let r = RunId::new();
        state.watch(t.id);
        state.apply(event(t.id, r, 1, "old"));
        state.apply(ServerMsg::Message {
            thread: t.id,
            message: message(t.id, "old", 1),
        });
        state.apply(status(&run(t.id, RunStatus::Working, 1)));
        state.apply(ServerMsg::Approval {
            thread: t.id,
            approval: approval(t.id, 1),
        });

        let p2 = project(9);
        let c2 = channel(p2.id);
        let t2 = thread(c2.id, "after restart", 9);
        let r2 = run(t2.id, RunStatus::Working, 9);
        state.apply(ServerMsg::Snapshot {
            projects: vec![p2],
            channels: vec![c2.clone()],
            threads: vec![t2.clone()],
            agents: vec![],
            runs: vec![r2.clone()],
            approvals: vec![],
        });
        assert!(state.threads(c.id).is_empty());
        assert_eq!(state.threads(c2.id)[0].id, t2.id);
        assert!(state.messages(t.id).is_empty());
        assert!(state.needs_you().is_empty());
        assert_eq!(state.running()[0].id, r2.id);
        assert_eq!(state.thread_status(t.id), ThreadStatus::Idle);
        // Still watched, rebuilt from the backlog crewd resends.
        assert!(state.events(t.id).is_empty());
        state.apply(event(t.id, r, 1, "old"));
        assert_eq!(texts(&state, t.id), ["old"]);
    }

    #[test]
    fn channel_status_follows_its_threads_only() {
        let (mut state, p, c, t) = one_thread();
        let other = channel(p.id);
        let elsewhere = thread(other.id, "elsewhere", 2);
        state.apply(ServerMsg::ChannelChanged {
            channel: other.clone(),
        });
        state.apply(ServerMsg::ThreadCreated {
            channel: other.id,
            thread: elsewhere.clone(),
        });
        state.apply(status(&run(elsewhere.id, RunStatus::Working, 1)));
        assert_eq!(state.channel_status(c.id), ThreadStatus::Idle);
        assert_eq!(state.channel_status(other.id), ThreadStatus::Working);
        state.apply(ServerMsg::Approval {
            thread: t.id,
            approval: approval(t.id, 1),
        });
        assert_eq!(state.channel_status(c.id), ThreadStatus::NeedsYou);
    }

    #[test]
    fn agent_status_comes_from_its_runs_and_their_asks() {
        let (mut state, _, _, t) = one_thread();
        let forge = agent("Forge");
        state.apply(ServerMsg::AgentChanged {
            agent: forge.clone(),
        });
        assert_eq!(state.agent_status(forge.id), ThreadStatus::Idle);
        let mut working = run(t.id, RunStatus::Working, 1);
        working.agent = forge.id;
        state.apply(status(&working));
        assert_eq!(state.agent_status(forge.id), ThreadStatus::Working);
        let mut ask = approval(t.id, 2);
        ask.run = working.id;
        state.apply(ServerMsg::Approval {
            thread: t.id,
            approval: ask,
        });
        assert_eq!(state.agent_status(forge.id), ThreadStatus::NeedsYou);
        // Someone else's run says nothing about Forge.
        assert_eq!(state.agent_status(AgentId::new()), ThreadStatus::Idle);
    }

    #[test]
    fn unread_counts_others_messages_after_a_time() {
        let (mut state, _, c, t) = one_thread();
        let mut reply = message(t.id, "done", 20);
        reply.author = Author::Agent(AgentId::new());
        let mut note = message(t.id, "Forge stopped.", 30);
        note.author = Author::System;
        for m in [message(t.id, "mine", 10), reply, note] {
            state.apply(ServerMsg::Message {
                thread: t.id,
                message: m,
            });
        }
        assert_eq!(state.unread(c.id, 0), 2, "your own message doesn't count");
        assert_eq!(state.unread(c.id, 20), 1);
        assert_eq!(state.unread(ChannelId::new(), 0), 0);
        assert_eq!(state.last_message(t.id).unwrap().body, "Forge stopped.");
    }

    #[test]
    fn working_agents_are_counted_once_and_queued_runs_not() {
        let (mut state, _, c, t) = one_thread();
        let other = thread(c.id, "other", 2);
        let forge = AgentId::new();
        let scout = AgentId::new();
        let mut a = run(t.id, RunStatus::Working, 1);
        a.agent = forge;
        let mut b = run(other.id, RunStatus::Working, 2);
        b.agent = forge;
        let mut q = run(t.id, RunStatus::Queued, 3);
        q.agent = scout;
        for r in [&a, &b, &q] {
            state.apply(status(r));
        }
        assert_eq!(state.working_agents(), [forge]);
    }

    #[test]
    fn live_runs_and_pending_approvals_are_per_thread() {
        let (mut state, _, c, t) = one_thread();
        let other = thread(c.id, "other", 2);
        state.apply(ServerMsg::ThreadCreated {
            channel: c.id,
            thread: other.clone(),
        });
        let later = run(t.id, RunStatus::Queued, 20);
        let first = run(t.id, RunStatus::Working, 10);
        for r in [
            &later,
            &first,
            &run(t.id, RunStatus::Done, 5),
            &run(other.id, RunStatus::Working, 1),
        ] {
            state.apply(status(r));
        }
        let ids: Vec<RunId> = state.live_runs(t.id).iter().map(|r| r.id).collect();
        assert_eq!(ids, [first.id, later.id]);

        let ask = approval(t.id, 3);
        let mut decided = approval(t.id, 1);
        decided.status = ApprovalStatus::Declined;
        for a in [&ask, &decided, &approval(other.id, 1)] {
            state.apply(ServerMsg::Approval {
                thread: a.thread,
                approval: a.clone(),
            });
        }
        let ids: Vec<ApprovalId> = state.pending_approvals(t.id).iter().map(|a| a.id).collect();
        assert_eq!(ids, [ask.id]);
    }

    #[test]
    fn a_threads_runs_and_approvals_include_finished_ones() {
        let (mut state, _, c, t) = one_thread();
        let other = thread(c.id, "other", 2);
        let late = run(t.id, RunStatus::Working, 20);
        let early = run(t.id, RunStatus::Done, 10);
        for r in [&late, &early, &run(other.id, RunStatus::Done, 1)] {
            state.apply(status(r));
        }
        let ids: Vec<RunId> = state.thread_runs(t.id).iter().map(|r| r.id).collect();
        assert_eq!(ids, [early.id, late.id]);

        let mut decided = approval(t.id, 1);
        decided.status = ApprovalStatus::Approved;
        let pending = approval(t.id, 2);
        for a in [&pending, &decided, &approval(other.id, 0)] {
            state.apply(ServerMsg::Approval {
                thread: a.thread,
                approval: a.clone(),
            });
        }
        let ids: Vec<ApprovalId> = state.thread_approvals(t.id).iter().map(|a| a.id).collect();
        assert_eq!(ids, [decided.id, pending.id]);
        assert_eq!(state.approval(decided.id), Some(&decided));
    }

    #[test]
    fn reconnecting_is_its_own_state() {
        let mut state = State::default();
        state.set_connection(ConnectionState::Reconnecting);
        assert_eq!(state.connection(), Some(&ConnectionState::Reconnecting));
    }

    #[test]
    fn connection_is_unknown_until_reported() {
        let mut state = State::default();
        assert_eq!(state.connection(), None);
        state.set_connection(ConnectionState::Connected);
        assert_eq!(state.connection(), Some(&ConnectionState::Connected));
    }
}
