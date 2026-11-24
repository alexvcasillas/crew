//! Wire protocol between crewd and its clients.
//!
//! Each frame is a big-endian `u32` length followed by that many bytes of JSON.

use std::io;

use crew_core::agent::{AgentSpec, Runtime};
use crew_core::claude::transcript::TranscriptItem;
use crew_core::event::{Event, RunStatus};
use crew_core::ids::{AgentId, ApprovalId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::policy::HandoffPolicy;
pub use crew_core::project_kind::{ProjectKind, WorkMode};
pub use crew_core::store::{Limits, RuntimeUsage};
use crew_core::store::{
    StoredAgent, StoredApproval, StoredChannel, StoredMessage, StoredProject, StoredRun,
    StoredThread,
};
pub use crew_core::worktree::{Diff, FileChange, RootInfo};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Version both sides must agree on in `Hello`.
pub const PROTO_VERSION: u32 = 14;
/// Largest frame body accepted, in bytes.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// A message from a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Request { id: u64, req: Request },
}

/// What a client asks crewd to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
    },
    /// Answered with the project's id. Refused if another project uses
    /// the folder; a removed one with the folder comes back instead. Any
    /// folder works. `main_branch` empty means the repo's current branch;
    /// `work_mode` unset means the default for what the folder holds.
    CreateProject {
        name: String,
        root: String,
        #[serde(default)]
        main_branch: String,
        #[serde(default)]
        work_mode: Option<WorkMode>,
        #[serde(default)]
        copy_files: Vec<String>,
        #[serde(default)]
        setup_command: Option<String>,
    },
    /// Answered with a [`FolderInfo`]: what the folder holds, for the Add
    /// project dialog. Reads the folder, never changes it.
    InspectFolder {
        path: String,
    },
    CreateChannel {
        project: ProjectId,
        name: String,
    },
    CreateAgent {
        spec: AgentSpec,
    },
    /// Replaces the agent's spec. Its next run uses it.
    UpdateAgent {
        agent: AgentId,
        spec: AgentSpec,
    },
    /// Deletes an agent that has no queued or working runs. Its messages
    /// and runs stay.
    DeleteAgent {
        agent: AgentId,
    },
    CreateThread {
        channel: ChannelId,
        title: String,
    },
    RenameThread {
        thread: ThreadId,
        title: String,
    },
    /// Archives the thread or brings it back. Its worktree and any live
    /// run are left alone. Answered with null; every client gets a
    /// [`ServerMsg::ThreadChanged`].
    ArchiveThread {
        thread: ThreadId,
        archived: bool,
    },
    /// Renames the channel and sets its description. The name is
    /// lowercase letters, numbers and dashes, and new in its project.
    /// Answered with null; every client gets a [`ServerMsg::ChannelChanged`].
    UpdateChannel {
        channel: ChannelId,
        name: String,
        description: String,
    },
    /// Answered with a [`ChannelOverview`]: stats and one summary per
    /// thread, archived ones included.
    ChannelOverview {
        channel: ChannelId,
    },
    PostMessage {
        thread: ThreadId,
        body: String,
    },
    Subscribe {
        thread: ThreadId,
    },
    /// Answered first, then a [`ServerMsg::Snapshot`], then every
    /// `RunStatus`, `Message` and `Approval` of all threads, plus
    /// `ThreadCreated` and `AgentChanged`. Events stay per thread: use
    /// `Subscribe` for those. Also `TerminalSessionChanged`.
    SubscribeAll,
    StopRun {
        run: RunId,
    },
    StopThread {
        thread: ThreadId,
    },
    StopAll,
    /// Saves the global limits and applies them at once. Answered with
    /// null; every client gets a [`ServerMsg::LimitsChanged`].
    SetLimits {
        limits: Limits,
    },
    /// Ended runs that started (any final status), newest first. `before`
    /// is an inclusive `ended_at` in Unix ms, so a page repeats the runs
    /// that end at its bound; drop them by id. `limit` is clamped to 1..=200.
    /// Answered with `Vec<StoredRun>`.
    RunHistory {
        before: Option<i64>,
        limit: u32,
    },
    /// Runs per runtime that ended at or after `since_ms`, plus live runs
    /// by their time so far. Answered with `Vec<RuntimeUsage>`.
    UsageSince {
        since_ms: i64,
    },
    ListRuns {
        status: Option<RunStatus>,
    },
    /// Answered with `Vec<StoredAgent>`. Deleted agents only come with
    /// `include_deleted`, for naming their past messages.
    ListAgents {
        #[serde(default)]
        include_deleted: bool,
    },
    /// Answered with `Vec<StoredProject>`.
    ListProjects,
    /// Answered with `Vec<StoredChannel>`.
    ListChannels {
        project: ProjectId,
    },
    /// Answered with `Vec<StoredThread>`.
    ListThreads {
        channel: ChannelId,
    },
    /// Sets what a new worktree of the project gets before its first run.
    UpdateProject {
        project: ProjectId,
        setup_command: Option<String>,
        copy_files: Vec<String>,
    },
    /// Answered with [`ThreadChanges`]: the uncommitted changes of the
    /// thread, per repo. Empty if it has none.
    ThreadDiff {
        thread: ThreadId,
    },
    /// Commits the thread's changes in one repo, `None` being the only
    /// one. In a copy per thread that is everything in the repo's
    /// worktree; in place, only the files the thread's runs changed, on
    /// the repo's current branch. Answered with the sha. Nothing is pushed.
    Commit {
        thread: ThreadId,
        #[serde(default)]
        repo: Option<String>,
        message: String,
    },
    /// Drops the thread's changes in `repos`, all of them if `None`.
    ///
    /// In a copy per thread: removes each repo's worktree, changes
    /// included, and its branch unless the branch has commits beyond the
    /// worktree's base (with `delete_branch`, the branch goes even then).
    /// The thread folder goes once no repo is left.
    ///
    /// In place: puts back only the tracked files the thread's runs
    /// changed. Files git doesn't have are never touched.
    ///
    /// Refused while a run in the thread is live.
    Discard {
        thread: ThreadId,
        #[serde(default)]
        repos: Option<Vec<String>>,
        #[serde(default)]
        delete_branch: bool,
    },
    /// From crew-mcp: the agent of `run` hands work to `agent`. Answered
    /// with a text for the agent.
    Handoff {
        run: RunId,
        agent: String,
        brief: String,
    },
    /// From crew-mcp: the agent of `run` wants to use `tool`. Answered with
    /// a [`PermissionAnswer`] once someone decides.
    PermissionAsk {
        run: RunId,
        tool: String,
        input: serde_json::Value,
        /// Claude's id for the tool call, if it sent one.
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    /// Answered with the pending `Vec<StoredApproval>`, for one thread or
    /// all of them.
    ListApprovals {
        thread: Option<ThreadId>,
    },
    /// Approves or declines a pending approval. `brief` replaces a
    /// handoff's brief.
    Decide {
        approval: ApprovalId,
        approve: bool,
        brief: Option<String>,
    },
    /// Lets `n` more handoffs run in the thread on their own.
    AllowMoreHandoffs {
        thread: ThreadId,
        n: u32,
    },
    /// Sets the thread's own handoff policy and budget. `None` falls back
    /// to the agent or channel.
    SetThreadHandoff {
        thread: ThreadId,
        policy: Option<HandoffPolicy>,
        budget: Option<u32>,
    },
    /// Sets the channel's handoff policy and budget. `None` falls back to
    /// the global default.
    SetChannelHandoff {
        channel: ChannelId,
        policy: Option<HandoffPolicy>,
        budget: Option<u32>,
    },
    /// Answered with `Vec<RuntimeInfo>`. Runs each CLI's `--version` and
    /// sign-in status command, and asks Codex for its models. If they take
    /// over a second, the answer says only which CLIs were found, and
    /// `ServerMsg::RuntimesChanged` follows with the rest.
    Runtimes,
    /// Answered first, then the session's last items as
    /// [`ServerMsg::TerminalEvent`]s, then new ones as they are written.
    /// After a [`ServerMsg::TerminalSessionReset`], send it again. Terminal
    /// sessions are read-only.
    SubscribeTerminal {
        session: String,
    },
    /// Stops the items of a terminal session.
    UnsubscribeTerminal {
        session: String,
    },
    /// Finds threads, channels, agents and files whose name holds `query`,
    /// ignoring case. Answered with [`SearchResults`]: each section best
    /// first, at most `limit` (1 to 200) long. `project` narrows it to one
    /// project; agents are found in any.
    Search {
        query: String,
        #[serde(default)]
        project: Option<ProjectId>,
        #[serde(default = "default_search_limit")]
        limit: u32,
    },
    /// Answered with a [`ProjectOverview`].
    ProjectOverview {
        project: ProjectId,
    },
    /// Answered with an [`AgentProfile`]. Deleted agents have one too.
    AgentProfile {
        agent: AgentId,
    },
    /// Answered with null; every client gets a [`ServerMsg::ProjectChanged`].
    RenameProject {
        project: ProjectId,
        name: String,
    },
    /// Removes the project from Crew. It leaves the snapshot, search and
    /// overviews; its runs stay in the run history. Its folder, branches
    /// and worktrees stay on disk. Refused while one of its runs is live;
    /// queued ones are dropped and pending approvals closed, each with a
    /// note. Adding its folder again brings it back with its channels and
    /// threads. Answered with null; every client gets a
    /// [`ServerMsg::ProjectRemoved`].
    RemoveProject {
        project: ProjectId,
    },
    /// Pins the project to the top of the sidebar, or unpins it. Answered
    /// with null; every client gets a [`ServerMsg::ProjectChanged`].
    PinProject {
        project: ProjectId,
        pinned: bool,
    },
    /// Answered with one [`ThreadSummary`] per thread that isn't
    /// archived, in `project` or in every project, and only in
    /// `channels` when given. Counts come from the channel overviews'
    /// cache; a [`ServerMsg::ChannelOverviewStale`] follows for a channel
    /// whose counts were still being worked out.
    ThreadSummaries {
        #[serde(default)]
        project: Option<ProjectId>,
        #[serde(default)]
        channels: Option<Vec<ChannelId>>,
    },
}

fn default_search_limit() -> u32 {
    50
}

/// Where a thread stands, for a row or a status pill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ThreadState {
    /// No runs yet.
    #[default]
    Idle,
    /// An approval waits for you.
    NeedsYou,
    Working {
        agent: AgentId,
    },
    Queued {
        agent: AgentId,
    },
    /// The last run ended like this.
    Done,
    Stopped,
    Failed,
}

/// The answer to [`Request::Search`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchResults {
    /// Archived threads are left out. Title matches come first, then
    /// threads whose messages hold the query.
    pub threads: Vec<ThreadHit>,
    pub channels: Vec<ChannelId>,
    /// Deleted agents are left out.
    pub agents: Vec<AgentId>,
    /// From git: tracked files and new ones that aren't ignored, in each
    /// project root and each worktree of a thread that isn't archived. A
    /// file that is unchanged in a worktree comes once per project.
    pub files: Vec<FileHit>,
    /// How many matched in each section before `limit`.
    pub totals: SearchTotals,
}

/// Matches per section of [`SearchResults`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchTotals {
    pub threads: u32,
    pub channels: u32,
    pub agents: u32,
    pub files: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadHit {
    pub thread: ThreadId,
    pub title: String,
    pub channel: ChannelId,
    #[serde(default)]
    pub state: ThreadState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileHit {
    pub project: ProjectId,
    /// The thread whose worktree holds it. `None` is the project root.
    pub thread: Option<ThreadId>,
    /// Relative to the root or worktree.
    pub path: String,
    /// It has uncommitted changes there, or is new.
    #[serde(default)]
    pub changed: bool,
}

/// The answer to [`Request::ProjectOverview`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectOverview {
    /// The project's own folder. When it no longer exists, only `exists`
    /// is set.
    pub root: RootInfo,
    /// Worktrees of its threads that are on disk.
    pub worktrees: u32,
    /// One per channel, in sidebar order.
    pub channels: Vec<ChannelRow>,
    /// The 10 threads with the latest activity, archived ones left out.
    pub recent_threads: Vec<RecentThread>,
    /// Agents with runs in the project, most runs this week first.
    /// Deleted agents included.
    pub agents: Vec<AgentUse>,
    /// The project's `copy_files`, as found in its folder.
    pub setup_files: Vec<SetupFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelRow {
    pub channel: ChannelId,
    #[serde(default)]
    pub stats: ChannelStats,
    /// Change counts are still being worked out; a
    /// [`ServerMsg::ChannelOverviewStale`] follows once they are.
    #[serde(default)]
    pub counting: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentThread {
    pub thread: ThreadId,
    pub channel: ChannelId,
    /// Agents with runs there, in the order they first ran.
    #[serde(default)]
    pub agents: Vec<AgentId>,
    /// Unix milliseconds.
    #[serde(default)]
    pub last_activity_at: i64,
    #[serde(default)]
    pub state: ThreadState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUse {
    pub agent: AgentId,
    #[serde(default)]
    pub runs_week: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupFile {
    /// As set in `copy_files`, relative to the project folder.
    pub path: String,
    #[serde(default)]
    pub size: SetupFileSize,
    /// Git ignores it.
    #[serde(default)]
    pub ignored: bool,
    /// It looks like it holds secrets, such as keys or production env
    /// files, so it starts left out.
    #[serde(default)]
    pub looks_secret: bool,
}

/// The answer to [`Request::InspectFolder`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderInfo {
    /// Whether the path is a folder. When not, the rest stays empty.
    pub exists: bool,
    /// The folder to add: the repo's top folder when the path is a
    /// folder inside a repo, else the path itself.
    pub root: String,
    pub kind: ProjectKind,
    /// The folder's name.
    pub name_suggestion: String,
    /// For a repo: its checked out branch and where `origin` points.
    pub branch: Option<String>,
    pub remote: Option<String>,
    /// Files and folders git ignores at the top of each repo, which a
    /// copy per thread would miss.
    pub setup_files: Vec<SetupFile>,
    /// The project that already uses this folder.
    pub already_added: Option<ProjectId>,
}

/// The answer to [`Request::ThreadDiff`].
///
/// `files` and `patch` cover every repo, paths relative to the project
/// (or thread) folder, so a client that knows one repo reads them as
/// before. `repos` splits them per repo.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadChanges {
    pub files: Vec<FileChange>,
    pub patch: String,
    pub repos: Vec<RepoChanges>,
    /// Files the thread's runs changed outside any repo. Git can't show
    /// or undo them.
    pub outside_git: Vec<OutsideFile>,
}

/// One repo's part of [`ThreadChanges`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoChanges {
    /// Relative to the project folder; empty when the project is the repo.
    pub repo: String,
    /// Where a commit lands.
    pub branch: Option<String>,
    /// Paths relative to the repo. The patch names them under the repo.
    pub diff: Diff,
    /// In place: files of `diff` the thread's runs didn't touch, changed
    /// outside this thread. Commit and discard leave them alone.
    pub outside_thread: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutsideFile {
    /// Relative to the project folder.
    pub path: String,
    /// Whether the file is still there.
    pub exists: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SetupFileSize {
    /// Not in the project folder.
    #[default]
    Missing,
    File {
        bytes: u64,
    },
    Folder {
        files: u32,
    },
}

/// The answer to [`Request::AgentProfile`]. "Week" means the last 7
/// days, "last week" the 7 before.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentProfile {
    pub stats: AgentStats,
    /// Projects it has runs in, removed ones left out.
    pub projects: u32,
    /// Its 20 latest runs that started, newest first. Runs in removed
    /// projects stay here.
    pub recent_runs: Vec<RunRow>,
    /// Threads it has runs in that aren't archived, of projects that
    /// aren't removed, latest activity first.
    pub threads: Vec<ThreadId>,
}

/// Numbers of an agent's profile. Runs count once they started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentStats {
    /// Runs that ended this week or are working.
    pub runs_week: u32,
    pub runs_last_week: u32,
    /// Runs that ended this week, and those of them that ended `Done`.
    pub finished_week: u32,
    pub clean_week: u32,
    /// Average time of the runs that ended this week, in ms.
    pub avg_run_ms: i64,
    /// What this week's runs edited, by their tool calls.
    pub files_week: u32,
    pub added_week: u32,
    pub removed_week: u32,
    /// Handoffs this week from this agent, and to it.
    pub handoffs_sent_week: u32,
    pub handoffs_received_week: u32,
}

/// One run in an agent's profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRow {
    pub run: StoredRun,
    /// How long it ran, or has run so far.
    #[serde(default)]
    pub ms: i64,
    /// What its edit and write calls touched. All zero for a run that
    /// only answered.
    #[serde(default)]
    pub files: u32,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    /// An approval of this run waits for you.
    #[serde(default)]
    pub waiting: bool,
    /// For a run in a removed project, where it ran, which clients no
    /// longer know. `None` otherwise.
    #[serde(default)]
    pub removed_project: Option<RunPlace>,
}

/// Where a run in a removed project ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunPlace {
    pub project: String,
    pub channel: String,
    pub thread_title: String,
}

/// A Claude Code session someone runs in their own terminal, inside one of
/// the projects. Crew only reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalSession {
    /// Claude Code's session id.
    pub id: String,
    pub project: ProjectId,
    /// Where it runs.
    pub cwd: String,
    /// The first line of the first prompt, at most 80 characters. Empty
    /// until there is one.
    pub title: String,
    /// Unix milliseconds.
    pub started_at: i64,
    /// Unix milliseconds of the last write to its file.
    pub last_active_at: i64,
    /// Whether Claude Code is working in it now.
    pub live: bool,
}

/// The answer to [`Request::ChannelOverview`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelOverview {
    pub stats: ChannelStats,
    /// One per thread of the channel, archived ones included, oldest first.
    pub threads: Vec<ThreadSummary>,
}

/// Numbers for a channel's header. Archived threads don't count. "Week"
/// means the last 7 days.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelStats {
    pub threads: u32,
    /// Threads created this week.
    pub threads_week: u32,
    /// Threads with a queued or working run.
    pub active: u32,
    /// Threads with an approval waiting for you.
    pub needs_you: u32,
    /// Time of runs that ended this week or are working, in ms.
    pub agent_ms_week: i64,
    pub runs_week: u32,
    /// Changes of threads active this week.
    pub files_week: u32,
    pub added_week: u32,
    pub removed_week: u32,
}

/// What a thread card shows.
///
/// `files`, `added` and `removed` are the thread's whole work: its
/// worktree against the base it started from, commits included. Review
/// changes (`ThreadDiff`) shows only what isn't committed yet, so after a
/// commit the card still counts files that Review changes no longer lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread: ThreadId,
    /// Messages from you and agents. System notes don't count.
    #[serde(default)]
    pub messages: u32,
    /// One per run, oldest first.
    #[serde(default)]
    pub runs: Vec<RunBar>,
    #[serde(default)]
    pub handoffs: u32,
    /// Agents with runs here, in the order they first ran.
    #[serde(default)]
    pub participants: Vec<AgentId>,
    /// Plain text, at most 240 characters: the pending ask, else the
    /// latest agent reply, else your first message.
    #[serde(default)]
    pub excerpt: String,
    /// Unix milliseconds.
    #[serde(default)]
    pub last_activity_at: i64,
    #[serde(default)]
    pub worktree: WorktreeState,
    /// Files changed against the worktree's base, commits included.
    #[serde(default)]
    pub files: u32,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    /// Time of all its runs, live ones so far, in ms.
    #[serde(default)]
    pub agent_ms: i64,
    /// The counts are still being worked out: `files`, `added` and
    /// `removed` are their last known values, or zeros.
    #[serde(default)]
    pub counting: bool,
}

/// One run in a thread card's strip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunBar {
    pub status: RunStatus,
    /// How long it ran, or has run so far. 0 if it never started.
    pub ms: i64,
}

/// Where a thread's worktree stands. A worktree whose folder is gone, or
/// that git can't read, is `Clean`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorktreeState {
    /// The thread has no worktree.
    #[default]
    None,
    Clean,
    /// Everything is committed on the branch.
    Committed,
    Uncommitted {
        files: u32,
    },
}

/// One agent CLI crewd can find. The answer to [`Request::Runtimes`] is a
/// `Vec` of these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub runtime: Runtime,
    pub installed: bool,
    /// As the CLI reports it, such as `2.1.0`.
    pub version: Option<String>,
    /// From the CLI's own status command. `None` if it has none or it
    /// couldn't tell.
    pub signed_in: Option<bool>,
    /// Where the binary is.
    pub path: Option<String>,
    /// Models an agent of this runtime can use. Fixed for Claude Code,
    /// asked of the CLI for Codex. Empty if unknown.
    #[serde(default)]
    pub models: Vec<ModelInfo>,
}

/// A model an agent can be set to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// What the agent's `model` is set to.
    pub id: String,
    /// The name to show.
    pub label: String,
}

/// The model a new Codex agent starts on: the first Luna model, the
/// cheapest, else the first listed.
pub fn default_codex_model(models: &[ModelInfo]) -> Option<&ModelInfo> {
    models
        .iter()
        .find(|m| m.id.contains("luna"))
        .or(models.first())
}

/// The answer to [`Request::PermissionAsk`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionAnswer {
    pub allow: bool,
    /// Why it was denied, shown to the agent.
    pub message: Option<String>,
}

/// A message from crewd.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Answer to the request with the same id.
    Response {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    /// Something a run did, with its sequence number in that run.
    Event {
        thread: ThreadId,
        run: RunId,
        seq: u64,
        event: Event,
    },
    /// A run changed state.
    RunStatus {
        thread: ThreadId,
        run: RunId,
        status: RunStatus,
        /// The run as saved after the change.
        details: StoredRun,
    },
    /// A message was added to a thread.
    Message {
        thread: ThreadId,
        message: StoredMessage,
    },
    /// An approval was asked for or decided.
    Approval {
        thread: ThreadId,
        approval: StoredApproval,
    },
    /// Everything a `SubscribeAll` client starts from. Agents include
    /// deleted ones, so past messages can name them. Runs are those queued
    /// or working, those that ended in the last hour, and those a pending
    /// approval points to. Approvals are the pending ones.
    Snapshot {
        projects: Vec<StoredProject>,
        channels: Vec<StoredChannel>,
        threads: Vec<StoredThread>,
        agents: Vec<StoredAgent>,
        runs: Vec<StoredRun>,
        approvals: Vec<StoredApproval>,
        /// Terminal sessions in the projects written to in the last 14
        /// days, most recent first.
        #[serde(default)]
        terminal_sessions: Vec<TerminalSession>,
        /// The global limits.
        #[serde(default)]
        limits: Limits,
    },
    /// The global limits were saved.
    LimitsChanged { limits: Limits },
    /// A terminal session appeared or changed.
    TerminalSessionChanged { session: TerminalSession },
    /// A terminal session's file was deleted, or it was not used for 14
    /// days.
    TerminalSessionRemoved { id: String },
    /// A subscribed terminal session's file was cut short or replaced.
    /// Drop its items and send `SubscribeTerminal` again for the new ones.
    TerminalSessionReset { session: String },
    /// One item of a subscribed terminal session, with its sequence number
    /// in that session.
    TerminalEvent {
        session: String,
        seq: u64,
        item: TranscriptItem,
    },
    /// A project was created or its settings changed.
    ProjectChanged { project: StoredProject },
    /// A project was removed from Crew: drop it with its channels and
    /// threads.
    ProjectRemoved { project: ProjectId },
    /// A channel was created or its settings changed.
    ChannelChanged { channel: StoredChannel },
    /// A thread was created.
    ThreadCreated {
        channel: ChannelId,
        thread: StoredThread,
    },
    /// A thread was renamed or its settings changed.
    ThreadChanged { thread: StoredThread },
    /// Change counts in the channel's overview were worked out or may
    /// have changed: ask for it again if it is shown.
    ChannelOverviewStale { channel: ChannelId },
    /// An agent was created, changed or deleted. Deleted agents have
    /// `deleted_at` set.
    AgentChanged { agent: StoredAgent },
    /// What crewd found about the agent CLIs, once a slow `Runtimes`
    /// check finished after its early answer.
    RuntimesChanged { runtimes: Vec<RuntimeInfo> },
    /// This connection fell behind and missed notifications. crewd closes
    /// it next; connect and subscribe again to catch up.
    Resync,
}

/// Writes one frame and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    let body = serde_json::to_vec(v)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame is too large"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

/// Reads one frame. Returns `None` on a clean end of input before a frame.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        let n = r.read(&mut len[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        got += n;
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes is over the {MAX_FRAME} byte limit"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip() {
        let msgs = vec![
            ClientMsg::Request {
                id: 1,
                req: Request::Hello {
                    version: PROTO_VERSION,
                },
            },
            ClientMsg::Request {
                id: 2,
                req: Request::StopAll,
            },
            ClientMsg::Request {
                id: 3,
                req: Request::ListRuns {
                    status: Some(RunStatus::Working),
                },
            },
            ClientMsg::Request {
                id: 4,
                req: Request::Handoff {
                    run: RunId::new(),
                    agent: "Sentinel".into(),
                    brief: "check the edit".into(),
                },
            },
            ClientMsg::Request {
                id: 5,
                req: Request::PermissionAsk {
                    run: RunId::new(),
                    tool: "Bash".into(),
                    input: serde_json::json!({ "command": "touch x.txt" }),
                    tool_use_id: Some("toolu_1".into()),
                },
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        for m in &msgs {
            write_frame(&mut buf, m).await.unwrap();
        }
        let first_len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        let first: serde_json::Value = serde_json::from_slice(&buf[4..4 + first_len]).unwrap();
        assert_eq!(first["type"], "request");

        let mut r = buf.as_slice();
        for m in &msgs {
            let got: ClientMsg = read_frame(&mut r).await.unwrap().unwrap();
            assert_eq!(&got, m);
        }
        assert!(read_frame::<_, ClientMsg>(&mut r).await.unwrap().is_none());

        let reply = ServerMsg::Response {
            id: 9,
            result: Err("nope".into()),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &reply).await.unwrap();
        let got: ServerMsg = read_frame(&mut buf.as_slice()).await.unwrap().unwrap();
        assert_eq!(got, reply);
    }

    #[test]
    fn channel_messages_round_trip() {
        let msgs = vec![
            ClientMsg::Request {
                id: 1,
                req: Request::ArchiveThread {
                    thread: ThreadId::new(),
                    archived: true,
                },
            },
            ClientMsg::Request {
                id: 2,
                req: Request::UpdateChannel {
                    channel: ChannelId::new(),
                    name: "bugs".into(),
                    description: "Things that broke".into(),
                },
            },
            ClientMsg::Request {
                id: 3,
                req: Request::ChannelOverview {
                    channel: ChannelId::new(),
                },
            },
            ClientMsg::Request {
                id: 4,
                req: Request::Discard {
                    thread: ThreadId::new(),
                    repos: Some(vec!["api".into()]),
                    delete_branch: true,
                },
            },
        ];
        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            assert_eq!(serde_json::from_str::<ClientMsg>(&json).unwrap(), msg);
        }
        // An older client's discard keeps the branch rule it knew.
        let thread = ThreadId::new();
        let json = format!(r#"{{"type":"discard","thread":"{thread}"}}"#);
        assert_eq!(
            serde_json::from_str::<Request>(&json).unwrap(),
            Request::Discard {
                thread,
                repos: None,
                delete_branch: false
            }
        );
        let overview = ChannelOverview {
            stats: ChannelStats {
                threads: 2,
                ..ChannelStats::default()
            },
            threads: vec![ThreadSummary {
                thread,
                messages: 3,
                runs: vec![RunBar {
                    status: RunStatus::Done,
                    ms: 1200,
                }],
                handoffs: 1,
                participants: vec![AgentId::new()],
                excerpt: "Done.".into(),
                last_activity_at: 5,
                worktree: WorktreeState::Uncommitted { files: 2 },
                files: 2,
                added: 3,
                removed: 1,
                agent_ms: 1200,
                counting: true,
            }],
        };
        let json = serde_json::to_value(&overview).unwrap();
        assert_eq!(
            json["threads"][0]["worktree"],
            serde_json::json!({ "state": "uncommitted", "files": 2 })
        );
        assert_eq!(
            serde_json::from_value::<ChannelOverview>(json).unwrap(),
            overview
        );
    }

    #[test]
    fn navigation_messages_round_trip() {
        let project = ProjectId::new();
        let reqs = vec![
            Request::Search {
                query: "parse".into(),
                project: Some(project),
                limit: 5,
            },
            Request::ProjectOverview { project },
            Request::AgentProfile {
                agent: AgentId::new(),
            },
            Request::RenameProject {
                project,
                name: "Demo".into(),
            },
            Request::RemoveProject { project },
        ];
        for req in reqs {
            let json = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        }
        let msg = ServerMsg::ProjectRemoved { project };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), msg);

        let agent = AgentId::new();
        let results = SearchResults {
            threads: vec![ThreadHit {
                thread: ThreadId::new(),
                title: "Fix the parser".into(),
                channel: ChannelId::new(),
                state: ThreadState::Working { agent },
            }],
            files: vec![FileHit {
                project,
                thread: None,
                path: "src/parser.rs".into(),
                changed: true,
            }],
            totals: SearchTotals {
                threads: 7,
                ..SearchTotals::default()
            },
            ..SearchResults::default()
        };
        let json = serde_json::to_value(&results).unwrap();
        assert_eq!(
            json["threads"][0]["state"],
            serde_json::json!({ "state": "working", "agent": agent })
        );
        assert_eq!(
            serde_json::from_value::<SearchResults>(json).unwrap(),
            results
        );
        // A search without a project searches them all.
        let req: Request =
            serde_json::from_str(r#"{"type":"search","query":"x","limit":3}"#).unwrap();
        assert!(matches!(req, Request::Search { project: None, .. }));
        let req: Request = serde_json::from_str(r#"{"type":"search","query":"x"}"#).unwrap();
        assert!(matches!(req, Request::Search { limit: 50, .. }));

        let overview = ProjectOverview {
            setup_files: vec![SetupFile {
                path: ".env".into(),
                size: SetupFileSize::File { bytes: 3 },
                ignored: true,
                looks_secret: false,
            }],
            ..ProjectOverview::default()
        };
        let json = serde_json::to_value(&overview).unwrap();
        assert_eq!(
            json["setup_files"][0]["size"],
            serde_json::json!({ "kind": "file", "bytes": 3 })
        );
        assert_eq!(
            serde_json::from_value::<ProjectOverview>(json).unwrap(),
            overview
        );
        let profile: AgentProfile = serde_json::from_str("{}").unwrap();
        assert_eq!(profile, AgentProfile::default());
    }

    #[test]
    fn folder_project_messages_round_trip() {
        let reqs = vec![
            Request::CreateProject {
                name: "platform".into(),
                root: "/work/platform".into(),
                main_branch: String::new(),
                work_mode: Some(WorkMode::InPlace),
                copy_files: vec!["api/.env".into()],
                setup_command: Some("make".into()),
            },
            Request::InspectFolder {
                path: "/work/platform".into(),
            },
            Request::Commit {
                thread: ThreadId::new(),
                repo: Some("api".into()),
                message: "fix".into(),
            },
        ];
        for req in reqs {
            let json = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        }
        // An older client's project and commit still read.
        let req: Request = serde_json::from_str(
            r#"{"type":"create_project","name":"demo","root":"/work/demo","main_branch":"main"}"#,
        )
        .unwrap();
        assert!(matches!(
            req,
            Request::CreateProject {
                work_mode: None,
                setup_command: None,
                ..
            }
        ));
        let thread = ThreadId::new();
        let json = format!(r#"{{"type":"commit","thread":"{thread}","message":"m"}}"#);
        assert!(matches!(
            serde_json::from_str::<Request>(&json).unwrap(),
            Request::Commit { repo: None, .. }
        ));

        let info = FolderInfo {
            exists: true,
            kind: ProjectKind::MultiRepo {
                repos: vec!["api".into(), "web".into()],
            },
            name_suggestion: "platform".into(),
            ..FolderInfo::default()
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["kind"]["kind"], "multi_repo");
        assert_eq!(serde_json::from_value::<FolderInfo>(json).unwrap(), info);

        // A client that reads a plain diff still reads the changes.
        let changes = ThreadChanges {
            files: vec![FileChange {
                path: "api/a.rs".into(),
                status: 'M',
                added: 1,
                removed: 0,
            }],
            patch: "diff --git a/api/a.rs b/api/a.rs\n".into(),
            repos: vec![RepoChanges {
                repo: "api".into(),
                branch: Some("main".into()),
                diff: Diff::default(),
                outside_thread: vec!["b.rs".into()],
            }],
            outside_git: vec![OutsideFile {
                path: "notes.md".into(),
                exists: true,
            }],
        };
        let json = serde_json::to_value(&changes).unwrap();
        let plain: Diff = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(plain.files, changes.files);
        assert_eq!(
            serde_json::from_value::<ThreadChanges>(json).unwrap(),
            changes
        );
    }

    #[test]
    fn permission_answer_json_shape() {
        let a: PermissionAnswer =
            serde_json::from_str(r#"{"allow":false,"message":"no"}"#).unwrap();
        assert_eq!(
            a,
            PermissionAnswer {
                allow: false,
                message: Some("no".into())
            }
        );
        let a: PermissionAnswer = serde_json::from_str(r#"{"allow":true,"message":null}"#).unwrap();
        assert!(a.allow);
    }

    #[test]
    fn terminal_messages_round_trip() {
        let msgs = vec![
            ServerMsg::TerminalSessionChanged {
                session: TerminalSession {
                    id: "s1".into(),
                    project: ProjectId::new(),
                    cwd: "/work/demo".into(),
                    title: "Fix the test".into(),
                    started_at: 1,
                    last_active_at: 2,
                    live: true,
                },
            },
            ServerMsg::TerminalSessionRemoved { id: "s1".into() },
            ServerMsg::TerminalSessionReset {
                session: "s1".into(),
            },
            ServerMsg::TerminalEvent {
                session: "s1".into(),
                seq: 3,
                item: TranscriptItem::UserPrompt("hi".into()),
            },
            ServerMsg::TerminalEvent {
                session: "s1".into(),
                seq: 4,
                item: TranscriptItem::Event(Event::Message { text: "ok".into() }),
            },
        ];
        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), msg);
        }
    }

    #[test]
    fn an_older_snapshot_has_no_terminal_sessions() {
        let json = r#"{"type":"snapshot","projects":[],"channels":[],"threads":[],
            "agents":[],"runs":[],"approvals":[]}"#;
        let ServerMsg::Snapshot {
            terminal_sessions,
            limits,
            ..
        } = serde_json::from_str(json).unwrap()
        else {
            panic!("not a snapshot");
        };
        assert!(terminal_sessions.is_empty());
        assert_eq!(limits, Limits::default());
    }

    #[test]
    fn limits_messages_round_trip() {
        let limits = Limits {
            max_runs: 2,
            paused: true,
            handoff_policy: HandoffPolicy::Budget,
            handoff_budget: 5,
        };
        let msg = ClientMsg::Request {
            id: 1,
            req: Request::SetLimits { limits },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ClientMsg>(&json).unwrap(), msg);
        let msg = ServerMsg::LimitsChanged { limits };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), msg);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let mut buf = (MAX_FRAME + 1).to_be_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        let err = read_frame::<_, serde_json::Value>(&mut buf.as_slice())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn eof_returns_none() {
        let empty: &[u8] = &[];
        let got = read_frame::<_, serde_json::Value>(&mut &*empty)
            .await
            .unwrap();
        assert!(got.is_none());

        // A cut-off length is an error, not a clean end.
        let partial: &[u8] = &[0, 0];
        let err = read_frame::<_, serde_json::Value>(&mut &*partial)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
