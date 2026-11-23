//! What the app knows about crewd, built only from what crewd sends. Pure:
//! no IO and no UI types, so it is tested on its own.

use std::collections::{HashMap, HashSet};

use crew_core::event::{Event, RunStatus};
use crew_core::ids::{AgentId, ApprovalId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::store::{
    ApprovalStatus, StoredAgent, StoredApproval, StoredChannel, StoredMessage, StoredProject,
    StoredRun, StoredThread,
};
use crew_proto::ServerMsg;

/// Whether the app can talk to crewd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
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

/// Details the notices don't carry, to be asked for with a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wants {
    /// An agent changed: ask for `ListAgents { include_deleted: true }`
    /// and pass the answer to [`State::set_agents`].
    pub agents: bool,
    /// A run is known only by its status: ask for the queued and working
    /// runs and pass them to [`State::merge_runs`].
    pub runs: bool,
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
    /// Runs heard of only through `RunStatus`: their thread and status.
    bare_runs: HashMap<RunId, (ThreadId, RunStatus)>,
    /// Sorted by `created_at`, ties in arrival order. Complete only for
    /// watched threads: other threads have what arrived since connecting.
    messages: HashMap<ThreadId, Vec<StoredMessage>>,
    /// Pending ones and those decided since connecting.
    approvals: HashMap<ApprovalId, StoredApproval>,
    /// One log per watched thread.
    logs: HashMap<ThreadId, EventLog>,
    wants: Wants,
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
            ServerMsg::AgentChanged { .. } => self.wants.agents = true,
            ServerMsg::RunStatus {
                thread,
                run,
                status,
            } => self.run_status(thread, run, status),
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
        self.bare_runs.clear();
        self.messages.clear();
        self.approvals = approvals.into_iter().map(|a| (a.id, a)).collect();
        for log in self.logs.values_mut() {
            *log = EventLog::default();
        }
        self.wants = Wants::default();
    }

    fn run_status(&mut self, thread: ThreadId, run: RunId, status: RunStatus) {
        if let Some(known) = self.runs.get_mut(&run) {
            if stage(&status) >= stage(&known.status) {
                known.status = status;
            }
            return;
        }
        match self.bare_runs.get_mut(&run) {
            Some((_, known)) if stage(&status) < stage(known) => {}
            Some((_, known)) => *known = status,
            None => {
                self.bare_runs.insert(run, (thread, status));
            }
        }
        self.wants.runs = true;
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

    /// What to ask crewd for, clearing it.
    pub fn take_wants(&mut self) -> Wants {
        std::mem::take(&mut self.wants)
    }

    /// The answer to `ListAgents { include_deleted: true }`.
    pub fn set_agents(&mut self, agents: Vec<StoredAgent>) {
        self.agents = agents.into_iter().map(|a| (a.id, a)).collect();
    }

    /// Runs from `ListRuns`. A run's status only moves forward, so an
    /// answer older than a notice doesn't undo it.
    pub fn merge_runs(&mut self, runs: Vec<StoredRun>) {
        for mut run in runs {
            let newer = match (self.runs.get(&run.id), self.bare_runs.remove(&run.id)) {
                (Some(known), _) => Some(known.status.clone()),
                (None, bare) => bare.map(|(_, status)| status),
            };
            if let Some(status) = newer.filter(|s| stage(s) > stage(&run.status)) {
                run.status = status;
            }
            self.runs.insert(run.id, run);
        }
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
            .map(|r| (r.thread, &r.status))
            .chain(self.bare_runs.values().map(|(t, s)| (*t, s)))
            .any(|(t, s)| is_live(s) && matches(t));
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
    use crew_core::store::{ApprovalKind, Author};
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

    fn status(thread: ThreadId, run: RunId, status: RunStatus) -> ServerMsg {
        ServerMsg::RunStatus {
            thread,
            run,
            status,
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
        state.merge_runs(vec![queued.clone()]);
        assert_eq!(state.thread_status(t.id), ThreadStatus::Working);
        state.apply(status(t.id, queued.id, RunStatus::Working));
        assert_eq!(state.running()[0].status, RunStatus::Working);
        state.apply(status(t.id, queued.id, RunStatus::Done));
        assert!(state.running().is_empty());
        assert_eq!(state.thread_status(t.id), ThreadStatus::Idle);
        // A late copy of an older status, as after a resubscribe, is ignored.
        state.apply(status(t.id, queued.id, RunStatus::Working));
        assert!(state.running().is_empty());
        assert_eq!(state.thread_status(t.id), ThreadStatus::Idle);
    }

    #[test]
    fn a_run_known_only_by_status_counts_and_asks_for_details() {
        let (mut state, _, _, t) = one_thread();
        assert_eq!(state.take_wants(), Wants::default());
        let r = run(t.id, RunStatus::Queued, 5);
        state.apply(status(t.id, r.id, RunStatus::Queued));
        assert_eq!(state.thread_status(t.id), ThreadStatus::Working);
        assert!(state.running().is_empty(), "no details yet");
        assert!(state.take_wants().runs);
        assert!(!state.take_wants().runs, "taken");

        // Working by now; the list answered while it was still queued.
        state.apply(status(t.id, r.id, RunStatus::Working));
        state.merge_runs(vec![r.clone()]);
        let running = state.running();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, r.id);
        assert_eq!(running[0].status, RunStatus::Working);
    }

    #[test]
    fn agent_changes_ask_for_the_agents() {
        let (mut state, _, _, _) = one_thread();
        let forge = agent("Forge");
        state.apply(ServerMsg::AgentChanged { agent: forge.id });
        assert!(state.take_wants().agents);
        state.set_agents(vec![forge.clone()]);
        assert_eq!(
            state.agent(forge.id).map(|a| a.spec.name.as_str()),
            Some("Forge")
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
        state.merge_runs(vec![run(busy.id, RunStatus::Working, 1)]);
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
        state.merge_runs(vec![b.clone(), done, a.clone()]);
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
        state.apply(status(t.id, RunId::new(), RunStatus::Working));
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
        assert_eq!(state.take_wants(), Wants::default());
        // Still watched, rebuilt from the backlog crewd resends.
        assert!(state.events(t.id).is_empty());
        state.apply(event(t.id, r, 1, "old"));
        assert_eq!(texts(&state, t.id), ["old"]);
    }

    #[test]
    fn connection_is_unknown_until_reported() {
        let mut state = State::default();
        assert_eq!(state.connection(), None);
        state.set_connection(ConnectionState::Connected);
        assert_eq!(state.connection(), Some(&ConnectionState::Connected));
    }
}
