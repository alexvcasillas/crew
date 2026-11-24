//! The `crew` command line client for crewd.

mod client;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use client::{Client, NotRunning};
use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::{Event, RunStatus, ToolKind};
use crew_core::ids::{AgentId, ApprovalId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::{
    ApprovalKind, ApprovalStatus, Author, StoredAgent, StoredApproval, StoredChannel,
    StoredProject, StoredRun, StoredThread, blocked_text, handoff_ask_text, paused_text,
    permission_text,
};
use crew_core::worktree::Diff;
use crew_proto::{Request, RuntimeInfo, ServerMsg};

#[derive(Parser)]
#[command(name = "crew", version, about = "Talk to crewd from the terminal")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a demo project in the current directory and print its thread id.
    InitDemo,
    /// Post a message to a thread.
    Say {
        /// A thread id, or a unique prefix of at least 4 characters.
        thread: String,
        #[arg(required = true, num_args = 1..)]
        body: Vec<String>,
    },
    /// Print a thread's history, then follow it live.
    Watch {
        /// A thread id, or a unique prefix of at least 4 characters.
        thread: String,
        /// Exit once no run in the thread is queued or working.
        #[arg(long)]
        until_idle: bool,
        /// With --until-idle: also exit as soon as an approval is pending.
        #[arg(long, requires = "until_idle")]
        until_ask: bool,
    },
    /// List queued and working runs.
    Status,
    /// Stop one run, all runs of a thread, or everything.
    Stop(StopArgs),
    /// List pending approvals, for one thread or all of them.
    Approvals {
        /// A thread id, or a unique prefix of at least 4 characters.
        thread: Option<String>,
    },
    /// Approve a pending handoff or permission ask.
    Approve {
        /// An approval id, or a unique prefix of at least 4 characters.
        approval: String,
        /// Replaces the brief of a handoff.
        #[arg(long)]
        brief: Option<String>,
    },
    /// Decline a pending handoff or permission ask.
    Decline {
        /// An approval id, or a unique prefix of at least 4 characters.
        approval: String,
    },
    /// Let more handoffs run in a thread without asking.
    AllowMore {
        /// A thread id, or a unique prefix of at least 4 characters.
        thread: String,
        n: u32,
    },
    /// Show the uncommitted changes in a thread's worktree.
    Diff {
        /// A thread id, or a unique prefix of at least 4 characters.
        thread: String,
        /// Print only the list of changed files.
        #[arg(long)]
        stat: bool,
    },
    /// Commit everything in a thread's worktree.
    Commit {
        /// A thread id, or a unique prefix of at least 4 characters.
        thread: String,
        #[arg(short, long)]
        message: String,
    },
    /// Remove a thread's worktree, changes included. Its branch goes too,
    /// unless it has commits.
    Discard {
        /// A thread id, or a unique prefix of at least 4 characters.
        thread: String,
        /// Confirm that the changes are thrown away.
        #[arg(long)]
        yes: bool,
    },
    /// Manage agents.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Manage threads.
    #[command(subcommand)]
    Thread(ThreadCommand),
    /// Manage channels.
    #[command(subcommand)]
    Channel(ChannelCommand),
}

#[derive(Subcommand)]
enum ThreadCommand {
    /// Set how handoffs in a thread are approved.
    Handoff {
        /// A thread id, or a unique prefix of at least 4 characters.
        thread: String,
        #[command(flatten)]
        setting: HandoffSetting,
    },
}

#[derive(Subcommand)]
enum ChannelCommand {
    /// Set how handoffs in a channel's threads are approved.
    Handoff {
        /// A channel id, or a unique prefix of at least 4 characters.
        channel: String,
        #[command(flatten)]
        setting: HandoffSetting,
    },
}

#[derive(Args)]
struct HandoffSetting {
    policy: PolicyArg,
    /// Handoffs that may run on their own before crew asks.
    #[arg(long)]
    budget: Option<u32>,
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Add an agent that runs the installed `claude` or `codex`.
    Add(AgentAdd),
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum RuntimeArg {
    Claude,
    Codex,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum PolicyArg {
    Ask,
    Budget,
    PerAgent,
}

impl From<PolicyArg> for HandoffPolicy {
    fn from(p: PolicyArg) -> Self {
        match p {
            PolicyArg::Ask => HandoffPolicy::AskFirst,
            PolicyArg::Budget => HandoffPolicy::Budget,
            PolicyArg::PerAgent => HandoffPolicy::PerAgent,
        }
    }
}

#[derive(Args)]
struct AgentAdd {
    name: String,
    /// The CLI that runs the agent.
    #[arg(long, value_enum, default_value = "claude")]
    runtime: RuntimeArg,
    /// The model. Needed for Claude Code; for Codex, defaults to the one
    /// Codex lists first, preferring Luna.
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    role: String,
    /// Let the agent edit files. Without it the agent is read-only.
    #[arg(long)]
    edit: bool,
    /// How this agent's handoff requests are approved.
    #[arg(long)]
    policy: Option<PolicyArg>,
    /// Handoffs this agent may start on their own. Needs `--policy budget`.
    #[arg(long, requires = "policy")]
    budget: Option<u32>,
    /// Stop a run after this many minutes. 0, the default, is no limit.
    #[arg(long, default_value_t = crew_core::agent::DEFAULT_MAX_RUN_MINUTES)]
    max_minutes: u32,
    #[arg(long, default_value = "")]
    instructions: String,
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("target").required(true)))]
struct StopArgs {
    /// The run to stop: an id, or a unique prefix of at least 4 characters.
    #[arg(group = "target")]
    run: Option<String>,
    /// Stop every run of this thread (id or unique prefix).
    #[arg(long, group = "target")]
    thread: Option<String>,
    /// Stop every run.
    #[arg(long, group = "target")]
    all: bool,
}

/// Parses a full id, which is a plain uuid string on the wire.
fn parse_full<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// Picks the one candidate that starts with `prefix`.
fn pick<T: Copy + ToString>(prefix: &str, ids: Vec<T>, what: &str, hint: &str) -> Result<T> {
    let valid = prefix.len() >= 4 && prefix.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if !valid {
        bail!("`{prefix}` is not a valid {what} id. {hint}");
    }
    let prefix = prefix.to_ascii_lowercase();
    let mut hits: Vec<T> = Vec::new();
    for id in ids {
        if id.to_string().starts_with(&prefix)
            && !hits.iter().any(|h| h.to_string() == id.to_string())
        {
            hits.push(id);
        }
    }
    match hits.len() {
        0 => bail!("No {what} matches `{prefix}`. {hint}"),
        1 => Ok(hits[0]),
        _ => bail!("`{prefix}` matches more than one {what}."),
    }
}

/// Resolves a full run id or a unique prefix of an active run.
async fn resolve_run(c: &mut Client, s: &str) -> Result<RunId> {
    if let Some(id) = parse_full(s) {
        return Ok(id);
    }
    let runs: Vec<StoredRun> = c.ask(Request::ListRuns { status: None }).await?;
    let active = runs.iter().filter(|r| is_active(&r.status)).map(|r| r.id);
    pick(s, active.collect(), "run", "Use the full run id.")
}

/// Every channel of every project.
async fn all_channels(c: &mut Client) -> Result<Vec<StoredChannel>> {
    let mut channels = Vec::new();
    let projects: Vec<StoredProject> = c.ask(Request::ListProjects).await?;
    for project in projects {
        let rows: Vec<StoredChannel> = c
            .ask(Request::ListChannels {
                project: project.id,
            })
            .await?;
        channels.extend(rows);
    }
    Ok(channels)
}

/// Resolves a full channel id or a unique prefix of any channel.
async fn resolve_channel(c: &mut Client, s: &str) -> Result<ChannelId> {
    if let Some(id) = parse_full(s) {
        return Ok(id);
    }
    let channels = all_channels(c).await?.into_iter().map(|ch| ch.id).collect();
    pick(s, channels, "channel", "Use the full channel id.")
}

/// Every thread of every project.
async fn all_threads(c: &mut Client) -> Result<Vec<StoredThread>> {
    let mut threads = Vec::new();
    for channel in all_channels(c).await? {
        let rows: Vec<StoredThread> = c
            .ask(Request::ListThreads {
                channel: channel.id,
            })
            .await?;
        threads.extend(rows);
    }
    Ok(threads)
}

/// Resolves a full thread id or a unique prefix of any thread.
async fn resolve_thread(c: &mut Client, s: &str) -> Result<ThreadId> {
    if let Some(id) = parse_full(s) {
        return Ok(id);
    }
    let threads = all_threads(c).await?.into_iter().map(|t| t.id).collect();
    pick(s, threads, "thread", "Use the full thread id.")
}

/// Resolves a full approval id or a unique prefix of a pending approval.
async fn resolve_approval(c: &mut Client, s: &str) -> Result<ApprovalId> {
    if let Some(id) = parse_full(s) {
        return Ok(id);
    }
    let pending: Vec<StoredApproval> = c.ask(Request::ListApprovals { thread: None }).await?;
    pick(
        s,
        pending.iter().map(|a| a.id).collect(),
        "approval",
        "Use the full approval id.",
    )
}

fn crew_home() -> Result<PathBuf> {
    let non_empty = |v: Option<OsString>| v.filter(|v| !v.is_empty());
    if let Some(home) = non_empty(std::env::var_os("CREW_HOME")) {
        return Ok(PathBuf::from(home));
    }
    match non_empty(std::env::var_os("HOME")) {
        Some(h) => Ok(PathBuf::from(h).join(".crew")),
        None => bail!("Set CREW_HOME or HOME."),
    }
}

fn short(id: impl ToString) -> String {
    id.to_string().chars().take(8).collect()
}

/// Agent names by id. `include_deleted` also names deleted agents, for
/// their past messages and runs.
async fn agent_names(c: &mut Client, include_deleted: bool) -> Result<HashMap<AgentId, String>> {
    let agents: Vec<StoredAgent> = c.ask(Request::ListAgents { include_deleted }).await?;
    Ok(agents.into_iter().map(|a| (a.id, a.spec.name)).collect())
}

async fn init_demo(c: &mut Client) -> Result<()> {
    let root = std::env::current_dir().context("Can't read the current directory.")?;
    let project: ProjectId = c
        .ask(Request::CreateProject {
            name: "demo".into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
        })
        .await?;
    let channel: ChannelId = c
        .ask(Request::CreateChannel {
            project,
            name: "general".into(),
        })
        .await?;
    let exists = agent_names(c, false).await?.values().any(|n| n == "Scout");
    if !exists {
        let _: AgentId = c
            .ask(Request::CreateAgent {
                spec: AgentSpec {
                    name: "Scout".into(),
                    role: "Reads the project and reports back".into(),
                    runtime: Runtime::ClaudeCode,
                    model: "haiku".into(),
                    instructions: String::new(),
                    permission_mode: PermissionMode::ReadOnly,
                    allowed_tools: vec![],
                    handoff_policy: None,
                    handoff_budget: None,
                    max_run_minutes: crew_core::agent::DEFAULT_MAX_RUN_MINUTES,
                },
            })
            .await?;
    }
    let thread: ThreadId = c
        .ask(Request::CreateThread {
            channel,
            title: "Demo".into(),
        })
        .await?;
    println!("{thread}");
    Ok(())
}

fn status_label(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "Queued",
        RunStatus::Working => "Working",
        RunStatus::Done => "Done",
        RunStatus::Failed => "Failed",
        RunStatus::Stopped => "Stopped",
        RunStatus::Interrupted => "Interrupted",
    }
}

fn is_active(status: &RunStatus) -> bool {
    matches!(status, RunStatus::Queued | RunStatus::Working)
}

async fn status(c: &mut Client) -> Result<()> {
    let names = agent_names(c, true).await?;
    let runs: Vec<StoredRun> = c.ask(Request::ListRuns { status: None }).await?;
    let mut any = false;
    for run in runs.iter().filter(|r| is_active(&r.status)) {
        any = true;
        let agent = names.get(&run.agent).map_or("?", String::as_str);
        println!(
            "{}  {}  {}  {}",
            short(run.id),
            agent,
            status_label(&run.status),
            short(run.thread)
        );
    }
    if !any {
        println!("Nothing running.");
    }
    Ok(())
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

/// How much of a raw line `watch` prints.
const RAW_CHARS: usize = 120;

/// The line to print for an event, without the agent prefix.
fn event_line(event: &Event) -> Option<String> {
    let with_target = |verb: &str, target: &Option<String>| match target {
        Some(t) => format!("{verb} {t}"),
        None => verb.to_string(),
    };
    match event {
        Event::ToolCall {
            name, kind, target, ..
        } => Some(match kind {
            ToolKind::Read => with_target("read", target),
            ToolKind::Search => with_target("search", target),
            ToolKind::Edit => with_target("edit", target),
            ToolKind::Create => with_target("create", target),
            ToolKind::Run => with_target("run", target),
            ToolKind::Other => with_target(name, target),
        }),
        Event::SessionStarted { model, .. } => Some(format!("started ({model})")),
        Event::Thinking { .. } => Some("thinking…".into()),
        Event::Message { text } => Some(format!("says: {}", first_line(text))),
        Event::PermissionDenied {
            tool,
            reason: Some(_),
            ..
        } => Some(format!("blocked {tool}")),
        Event::PermissionDenied { tool, .. } => Some(format!("denied {tool}")),
        Event::Error { message } => Some(format!("error: {message}")),
        Event::Raw { line } => Some(format!(
            "raw: {}",
            line.chars().take(RAW_CHARS).collect::<String>()
        )),
        Event::ToolResult {
            is_error: true,
            output,
            ..
        } => Some(format!("tool failed: {}", first_line(output))),
        Event::ToolResult { .. } | Event::TurnEnd { .. } | Event::Usage { .. } => None,
    }
}

fn status_line(status: &RunStatus) -> Option<String> {
    match status {
        RunStatus::Done => Some("✓ done".into()),
        RunStatus::Failed => Some("✗ failed: the run ended with an error".into()),
        RunStatus::Stopped => Some("■ stopped".into()),
        RunStatus::Interrupted => Some("✗ failed: crewd restarted during the run".into()),
        _ => None,
    }
}

/// Keeps agent names and failure reasons while watching.
struct Names {
    agents: HashMap<AgentId, String>,
    runs: HashMap<RunId, AgentId>,
}

impl Names {
    async fn load(c: &mut Client) -> Result<Names> {
        let runs: Vec<StoredRun> = c.ask(Request::ListRuns { status: None }).await?;
        Ok(Names {
            agents: agent_names(c, true).await?,
            runs: runs.into_iter().map(|r| (r.id, r.agent)).collect(),
        })
    }

    async fn of_run(&mut self, ctl: &mut Client, run: RunId) -> Result<String> {
        if !self.runs.contains_key(&run) {
            *self = Names::load(ctl).await?;
        }
        Ok(self
            .runs
            .get(&run)
            .and_then(|a| self.agents.get(a))
            .cloned()
            .unwrap_or_else(|| "?".into()))
    }

    async fn of_agent(&mut self, ctl: &mut Client, agent: AgentId) -> Result<String> {
        if !self.agents.contains_key(&agent) {
            *self = Names::load(ctl).await?;
        }
        Ok(self
            .agents
            .get(&agent)
            .cloned()
            .unwrap_or_else(|| "?".into()))
    }
}

/// Fallback so `--until-idle` can't hang if a follow-up message never comes.
const IDLE_FALLBACK: std::time::Duration = std::time::Duration::from_secs(10);

/// Tracks which runs owe the thread a chat message, so `--until-idle` can
/// wait for them instead of guessing. A run that ends `Done` owes an agent
/// message, any other run a system message.
#[derive(Default)]
struct Followups {
    /// Runs that ended, and whether the message is from the agent.
    expected: HashMap<RunId, bool>,
    /// Runs seen with a message, and whether it was from the agent.
    seen: HashSet<(RunId, bool)>,
}

impl Followups {
    fn complete(&self) -> bool {
        self.expected
            .iter()
            .all(|(run, agent)| self.seen.contains(&(*run, *agent)))
    }
}

/// What an approval asks, as crewd words it in the thread.
async fn approval_text(
    names: &mut Names,
    ctl: &mut Client,
    approval: &StoredApproval,
) -> Result<String> {
    Ok(match &approval.kind {
        ApprovalKind::Permission { tool, input, .. } => {
            let name = names.of_run(ctl, approval.run).await?;
            permission_text(&name, tool, input)
        }
        ApprovalKind::Blocked { tool, input, .. } => {
            let name = names.of_run(ctl, approval.run).await?;
            blocked_text(&name, tool, input)
        }
        ApprovalKind::Handoff {
            from, to, paused, ..
        } => {
            if *paused {
                let used = all_threads(ctl)
                    .await?
                    .into_iter()
                    .find(|t| t.id == approval.thread)
                    .map_or(0, |t| t.handoffs_used);
                paused_text(used)
            } else {
                let from = names.of_agent(ctl, *from).await?;
                let to = names.of_agent(ctl, *to).await?;
                handoff_ask_text(&from, &to)
            }
        }
    })
}

/// True when the thread has nothing running and nothing waiting on you.
/// Asked of crewd, which answers once the run's end is fully recorded, so
/// a handoff ask made as the run ended is already there.
async fn settled(ctl: &mut Client, thread: ThreadId) -> Result<bool> {
    let waiting: Vec<StoredApproval> = ctl
        .ask(Request::ListApprovals {
            thread: Some(thread),
        })
        .await?;
    Ok(waiting.is_empty() && idle(ctl, thread).await?)
}

async fn watch(
    home: &std::path::Path,
    thread: ThreadId,
    until_idle: bool,
    until_ask: bool,
) -> Result<()> {
    let mut sub = Client::connect(home).await?;
    // A second connection answers lookups so the stream stays in order.
    let mut ctl = Client::connect(home).await?;
    let mut names = Names::load(&mut ctl).await?;
    // The reason a run failed, taken from its last error or failed turn.
    let mut reasons: HashMap<RunId, String> = HashMap::new();
    let mut asked: HashSet<ApprovalId> = HashSet::new();
    sub.call(Request::Subscribe { thread }).await?;
    // Crewd answers this only after it wrote the thread's history, so the
    // history is what is buffered now.
    sub.call(Request::ListRuns { status: None }).await?;
    let mut backlog = sub.buffered();
    // Once idle, keep reading until every finished run has its chat
    // message, since those arrive after the run backlog.
    let mut followups = Followups::default();
    let mut idle_since: Option<tokio::time::Instant> = None;
    loop {
        if idle_since.is_some() && backlog == 0 && followups.complete() {
            return Ok(());
        }
        let next = match idle_since {
            Some(since) => match tokio::time::timeout_at(since + IDLE_FALLBACK, sub.next()).await {
                Ok(next) => next?,
                Err(_) => return Ok(()),
            },
            None => sub.next().await?,
        };
        let Some(msg) = next else {
            bail!("crewd closed the connection.");
        };
        backlog = backlog.saturating_sub(1);
        match msg {
            ServerMsg::Event { run, event, .. } => {
                match &event {
                    Event::Error { message } => {
                        reasons.insert(run, message.clone());
                    }
                    Event::TurnEnd {
                        ok: false, summary, ..
                    } => {
                        reasons.insert(run, summary.clone());
                    }
                    _ => {}
                }
                if let Some(line) = event_line(&event) {
                    let name = names.of_run(&mut ctl, run).await?;
                    println!("[{name}] {line}");
                }
            }
            ServerMsg::RunStatus { run, status, .. } => {
                if let Some(mut line) = status_line(&status) {
                    if status == RunStatus::Failed
                        && let Some(reason) = reasons.get(&run)
                    {
                        line = format!("✗ failed: {}", first_line(reason));
                    }
                    let name = names.of_run(&mut ctl, run).await?;
                    println!("[{name}] {line}");
                    match status {
                        RunStatus::Done => {
                            followups.expected.insert(run, true);
                        }
                        RunStatus::Failed => {
                            followups.expected.insert(run, false);
                        }
                        // A run stopped before it started gets no message
                        // of its own, only the thread-wide one.
                        RunStatus::Stopped if started(&mut ctl, run).await? => {
                            followups.expected.insert(run, false);
                        }
                        _ => {}
                    }
                    if until_idle {
                        update_idle(&mut ctl, thread, &mut idle_since).await?;
                    }
                } else {
                    idle_since = None;
                }
            }
            ServerMsg::Message { message, .. } => {
                // Live, a run's last message comes just before its final
                // status, so messages are noted whenever they arrive.
                if let Some(run) = message.run {
                    followups
                        .seen
                        .insert((run, matches!(message.author, Author::Agent(_))));
                }
                let author = match message.author {
                    Author::You => "you".to_string(),
                    Author::System => "system".to_string(),
                    Author::Agent(a) => names.of_agent(&mut ctl, a).await?,
                };
                println!("{author}: {}", message.body);
            }
            ServerMsg::Approval { approval, .. } => match approval.status {
                ApprovalStatus::Pending => {
                    if asked.insert(approval.id) {
                        let text = approval_text(&mut names, &mut ctl, &approval).await?;
                        println!("[?] {text}  (crew approve {})", short(approval.id));
                    }
                    idle_since = None;
                    if until_ask {
                        return Ok(());
                    }
                }
                ref decided => {
                    println!(
                        "{}",
                        match decided {
                            ApprovalStatus::Approved => "[✓] approved",
                            ApprovalStatus::Declined => "[✗] declined",
                            _ => "[■] cancelled",
                        }
                    );
                    if until_idle {
                        update_idle(&mut ctl, thread, &mut idle_since).await?;
                    }
                }
            },
            // Responses are read elsewhere, the rest only reach SubscribeAll.
            ServerMsg::Response { .. }
            | ServerMsg::Snapshot { .. }
            | ServerMsg::ProjectChanged { .. }
            | ServerMsg::ChannelChanged { .. }
            | ServerMsg::ThreadCreated { .. }
            | ServerMsg::ThreadChanged { .. }
            | ServerMsg::ChannelOverviewStale { .. }
            | ServerMsg::AgentChanged { .. }
            | ServerMsg::RuntimesChanged { .. }
            | ServerMsg::LimitsChanged { .. }
            | ServerMsg::TerminalSessionChanged { .. }
            | ServerMsg::TerminalEvent { .. }
            | ServerMsg::TerminalSessionRemoved { .. }
            | ServerMsg::TerminalSessionReset { .. } => {}
            ServerMsg::Resync => bail!("watch fell behind crewd. Run it again."),
        }
    }
}

/// Starts or cancels the idle countdown after something changed.
async fn update_idle(
    ctl: &mut Client,
    thread: ThreadId,
    idle_since: &mut Option<tokio::time::Instant>,
) -> Result<()> {
    if settled(ctl, thread).await? {
        idle_since.get_or_insert_with(tokio::time::Instant::now);
    } else {
        *idle_since = None;
    }
    Ok(())
}

/// True when the run got as far as starting `claude`.
async fn started(ctl: &mut Client, run: RunId) -> Result<bool> {
    let runs: Vec<StoredRun> = ctl.ask(Request::ListRuns { status: None }).await?;
    Ok(runs.iter().any(|r| r.id == run && !r.session_id.is_empty()))
}

/// True when the thread has no queued or working run.
async fn idle(ctl: &mut Client, thread: ThreadId) -> Result<bool> {
    let runs: Vec<StoredRun> = ctl.ask(Request::ListRuns { status: None }).await?;
    Ok(!runs
        .iter()
        .any(|r| r.thread == thread && is_active(&r.status)))
}

async fn run(cli: Cli) -> Result<()> {
    let home = crew_home()?;
    let mut c = Client::connect(&home).await?;
    match cli.command {
        Command::InitDemo => init_demo(&mut c).await,
        Command::Say { thread, body } => {
            let thread = resolve_thread(&mut c, &thread).await?;
            let _: crew_core::ids::MessageId = c
                .ask(Request::PostMessage {
                    thread,
                    body: body.join(" "),
                })
                .await?;
            Ok(())
        }
        Command::Watch {
            thread,
            until_idle,
            until_ask,
        } => {
            let thread = resolve_thread(&mut c, &thread).await?;
            drop(c);
            watch(&home, thread, until_idle, until_ask).await
        }
        Command::Status => status(&mut c).await,
        Command::Stop(args) => {
            let req = match (args.run, args.thread) {
                (Some(run), _) => Request::StopRun {
                    run: resolve_run(&mut c, &run).await?,
                },
                (None, Some(thread)) => Request::StopThread {
                    thread: resolve_thread(&mut c, &thread).await?,
                },
                (None, None) => Request::StopAll,
            };
            c.call(req).await?;
            Ok(())
        }
        Command::Approvals { thread } => {
            let thread = match thread {
                Some(t) => Some(resolve_thread(&mut c, &t).await?),
                None => None,
            };
            approvals(&mut c, thread).await
        }
        Command::Approve { approval, brief } => {
            let approval = resolve_approval(&mut c, &approval).await?;
            c.call(Request::Decide {
                approval,
                approve: true,
                brief,
            })
            .await?;
            Ok(())
        }
        Command::Decline { approval } => {
            let approval = resolve_approval(&mut c, &approval).await?;
            c.call(Request::Decide {
                approval,
                approve: false,
                brief: None,
            })
            .await?;
            Ok(())
        }
        Command::AllowMore { thread, n } => {
            let thread = resolve_thread(&mut c, &thread).await?;
            c.call(Request::AllowMoreHandoffs { thread, n }).await?;
            Ok(())
        }
        Command::Diff { thread, stat } => {
            let thread = resolve_thread(&mut c, &thread).await?;
            let diff: Diff = c.ask(Request::ThreadDiff { thread }).await?;
            print_diff(&diff, stat);
            Ok(())
        }
        Command::Commit { thread, message } => {
            let thread = resolve_thread(&mut c, &thread).await?;
            let sha: String = c.ask(Request::Commit { thread, message }).await?;
            println!("{}", sha.chars().take(8).collect::<String>());
            Ok(())
        }
        Command::Discard { thread, yes } => {
            if !yes {
                bail!("Discard removes the worktree, changes included. Add --yes to confirm.");
            }
            let thread = resolve_thread(&mut c, &thread).await?;
            c.call(Request::Discard {
                thread,
                delete_branch: false,
            })
            .await?;
            Ok(())
        }
        Command::Thread(ThreadCommand::Handoff { thread, setting }) => {
            let thread = resolve_thread(&mut c, &thread).await?;
            c.call(Request::SetThreadHandoff {
                thread,
                policy: Some(setting.policy.into()),
                budget: setting.budget,
            })
            .await?;
            Ok(())
        }
        Command::Channel(ChannelCommand::Handoff { channel, setting }) => {
            let channel = resolve_channel(&mut c, &channel).await?;
            c.call(Request::SetChannelHandoff {
                channel,
                policy: Some(setting.policy.into()),
                budget: setting.budget,
            })
            .await?;
            Ok(())
        }
        Command::Agent(AgentCommand::Add(a)) => {
            if a.budget.is_some() && a.policy != Some(PolicyArg::Budget) {
                bail!("--budget needs --policy budget.");
            }
            let (runtime, model) = match (a.runtime, a.model) {
                (RuntimeArg::Claude, Some(model)) => (Runtime::ClaudeCode, model),
                (RuntimeArg::Claude, None) => {
                    bail!("A Claude Code agent needs --model, such as --model sonnet.")
                }
                (RuntimeArg::Codex, Some(model)) => (Runtime::Codex, model),
                (RuntimeArg::Codex, None) => {
                    let model = default_codex_model(&mut c).await?;
                    println!("{} uses {} ({}).", a.name, model.id, model.label);
                    (Runtime::Codex, model.id)
                }
            };
            let spec = AgentSpec {
                name: a.name,
                role: a.role,
                runtime,
                model,
                instructions: a.instructions,
                permission_mode: if a.edit {
                    PermissionMode::EditFiles
                } else {
                    PermissionMode::ReadOnly
                },
                allowed_tools: vec![],
                handoff_policy: a.policy.map(HandoffPolicy::from),
                handoff_budget: a.budget,
                max_run_minutes: a.max_minutes,
            };
            let _: AgentId = c.ask(Request::CreateAgent { spec }).await?;
            Ok(())
        }
    }
}

/// How long `agent add` waits for Codex to list its models after crewd's
/// quick answer.
const CODEX_MODELS_WAIT: std::time::Duration = std::time::Duration::from_secs(12);

/// The model a new Codex agent starts on, from the models crewd lists.
async fn default_codex_model(c: &mut Client) -> Result<crew_proto::ModelInfo> {
    // A slow check is answered with what is on PATH, and the rest comes
    // only to `SubscribeAll` connections.
    c.call(Request::SubscribeAll).await?;
    let quick: Vec<RuntimeInfo> = c.ask(Request::Runtimes).await?;
    let mut codex = quick.into_iter().find(|r| r.runtime == Runtime::Codex);
    let partial = |r: &RuntimeInfo| r.installed && r.models.is_empty() && r.signed_in.is_none();
    if codex.as_ref().is_some_and(partial) {
        let full = tokio::time::timeout(CODEX_MODELS_WAIT, async {
            loop {
                match c.next().await? {
                    Some(ServerMsg::RuntimesChanged { runtimes }) => return Ok(Some(runtimes)),
                    Some(_) => {}
                    None => return Ok::<_, anyhow::Error>(None),
                }
            }
        })
        .await;
        if let Ok(Ok(Some(runtimes))) = full {
            codex = runtimes.into_iter().find(|r| r.runtime == Runtime::Codex);
        }
    }
    match codex {
        Some(r) if !r.installed => {
            bail!("Codex isn't installed. Install it, or use --runtime claude.")
        }
        Some(r) if r.signed_in == Some(false) => {
            bail!("Codex isn't signed in. Run `codex login` in a terminal, then try again.")
        }
        Some(r) => crew_proto::default_codex_model(&r.models)
            .cloned()
            .context("Codex listed no models. Pass one with --model, or try again in a moment."),
        None => bail!("crewd doesn't know Codex. Pass a model with --model."),
    }
}

async fn approvals(c: &mut Client, thread: Option<ThreadId>) -> Result<()> {
    let mut names = Names::load(c).await?;
    let pending: Vec<StoredApproval> = c.ask(Request::ListApprovals { thread }).await?;
    if pending.is_empty() {
        println!("Nothing waiting.");
    }
    for a in &pending {
        let text = approval_text(&mut names, c, a).await?;
        println!("{}  {}  {text}", short(a.id), short(a.thread));
    }
    Ok(())
}

fn print_diff(diff: &Diff, stat: bool) {
    if diff.files.is_empty() {
        println!("No changes.");
        return;
    }
    for f in &diff.files {
        println!("{} {} +{} -{}", f.status, f.path, f.added, f.removed);
    }
    if !stat && !diff.patch.is_empty() {
        println!();
        print!("{}", diff.patch);
        if !diff.patch.ends_with('\n') {
            println!();
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("crew: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if e.downcast_ref::<NotRunning>().is_some() {
                eprintln!("{e}");
            } else {
                eprintln!("crew: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_shows_denials_errors_raw_lines_and_failed_tools() {
        let denied = Event::PermissionDenied {
            tool: "Bash".into(),
            input: serde_json::json!({"command": "rm -rf x"}),
            reason: None,
        };
        assert_eq!(event_line(&denied).as_deref(), Some("denied Bash"));
        let blocked = Event::PermissionDenied {
            tool: "Bash".into(),
            input: serde_json::json!({"command": "rm -rf x"}),
            reason: Some("Destructive".into()),
        };
        assert_eq!(event_line(&blocked).as_deref(), Some("blocked Bash"));

        let error = Event::Error {
            message: "rate limited".into(),
        };
        assert_eq!(event_line(&error).as_deref(), Some("error: rate limited"));

        let long = "x".repeat(130);
        let raw = Event::Raw { line: long };
        assert_eq!(event_line(&raw), Some(format!("raw: {}", "x".repeat(120))));
        let short_raw = Event::Raw {
            line: "é-not json".into(),
        };
        assert_eq!(event_line(&short_raw).as_deref(), Some("raw: é-not json"));

        let failed = Event::ToolResult {
            id: "t1".into(),
            is_error: true,
            output: "No such file\nmore detail".into(),
        };
        assert_eq!(
            event_line(&failed).as_deref(),
            Some("tool failed: No such file")
        );
        let ok = Event::ToolResult {
            id: "t1".into(),
            is_error: false,
            output: "fine".into(),
        };
        assert_eq!(event_line(&ok), None);
    }
}
