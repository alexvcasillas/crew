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
    ApprovalKind, ApprovalStatus, Author, StoredApproval, StoredChannel, StoredProject, StoredRun,
    StoredThread,
};
use crew_core::worktree::Diff;
use crew_proto::{Request, ServerMsg};

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
    /// Remove a thread's worktree and branch, changes included.
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
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Add an agent that runs the installed `claude`.
    Add(AgentAdd),
}

#[derive(Clone, Copy, ValueEnum)]
enum PolicyArg {
    Ask,
    Budget,
    PerAgent,
}

#[derive(Args)]
struct AgentAdd {
    name: String,
    #[arg(long)]
    model: String,
    #[arg(long)]
    role: String,
    /// Let the agent edit files. Without it the agent is read-only.
    #[arg(long)]
    edit: bool,
    /// How this agent's handoff requests are approved.
    #[arg(long)]
    policy: Option<PolicyArg>,
    /// Handoffs this agent may start on their own.
    #[arg(long)]
    budget: Option<u32>,
    /// Stop a run after this many minutes.
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

/// Resolves a full thread id or a unique prefix of any thread.
async fn resolve_thread(c: &mut Client, s: &str) -> Result<ThreadId> {
    if let Some(id) = parse_full(s) {
        return Ok(id);
    }
    let mut threads: Vec<ThreadId> = Vec::new();
    let projects: Vec<StoredProject> = c.ask(Request::ListProjects).await?;
    for project in projects {
        let channels: Vec<StoredChannel> = c
            .ask(Request::ListChannels {
                project: project.id,
            })
            .await?;
        for channel in channels {
            let rows: Vec<StoredThread> = c
                .ask(Request::ListThreads {
                    channel: channel.id,
                })
                .await?;
            threads.extend(rows.into_iter().map(|t| t.id));
        }
    }
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

async fn agent_names(c: &mut Client) -> Result<HashMap<AgentId, String>> {
    let agents: Vec<(AgentId, AgentSpec)> = c.ask(Request::ListAgents).await?;
    Ok(agents.into_iter().map(|(id, a)| (id, a.name)).collect())
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
    let exists = agent_names(c).await?.values().any(|n| n == "Scout");
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
                    max_run_minutes: 20,
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
    let names = agent_names(c).await?;
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
        Event::ToolResult { .. } | Event::TurnEnd { .. } => None,
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
            agents: agent_names(c).await?,
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
/// Once every follow-up is in, how long the stream must stay quiet before
/// `--until-idle` exits. The history of a thread arrives in one burst.
const QUIET: std::time::Duration = std::time::Duration::from_millis(300);

/// Tracks which runs owe the thread a chat message, so `--until-idle` can
/// wait for them instead of guessing.
#[derive(Default)]
struct Followups {
    expected: HashSet<RunId>,
    seen: HashSet<RunId>,
}

impl Followups {
    fn complete(&self) -> bool {
        self.expected.is_subset(&self.seen)
    }
}

/// A short, one-line view of a tool's input: its command or path if it
/// has one, else its JSON.
fn short_input(input: &serde_json::Value) -> String {
    let text = ["command", "file_path", "path", "url", "pattern"]
        .iter()
        .find_map(|k| input.get(k).and_then(serde_json::Value::as_str))
        .map(String::from)
        .unwrap_or_else(|| match input {
            serde_json::Value::Null => String::new(),
            serde_json::Value::Object(map) if map.is_empty() => String::new(),
            other => other.to_string(),
        });
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let cut: String = first.chars().take(80).collect();
    if cut.len() < first.len() || lines.next().is_some() {
        format!("{cut}...")
    } else {
        cut
    }
}

/// What an approval asks, as crewd words it in the thread.
async fn approval_text(
    names: &mut Names,
    ctl: &mut Client,
    approval: &StoredApproval,
) -> Result<String> {
    Ok(match &approval.kind {
        ApprovalKind::Permission { tool, input } => {
            let name = names.of_run(ctl, approval.run).await?;
            let shown = short_input(input);
            if shown.is_empty() {
                format!("{name} wants to run {tool}.")
            } else {
                format!("{name} wants to run {tool}: {shown}")
            }
        }
        ApprovalKind::Handoff {
            from, to, paused, ..
        } => {
            let from = names.of_agent(ctl, *from).await?;
            let to = names.of_agent(ctl, *to).await?;
            if *paused {
                format!("{from} wants to bring in {to}. The handoff budget is used up.")
            } else {
                format!("{from} wants to bring in {to}.")
            }
        }
    })
}

/// True when the thread has nothing running and nothing waiting on you.
async fn settled(
    ctl: &mut Client,
    thread: ThreadId,
    pending: &HashSet<ApprovalId>,
) -> Result<bool> {
    Ok(pending.is_empty() && idle(ctl, thread).await?)
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
    // Approvals waiting on you. The backlog reports them after the runs
    // finish, so they are read first.
    let waiting: Vec<StoredApproval> = ctl
        .ask(Request::ListApprovals {
            thread: Some(thread),
        })
        .await?;
    let mut pending: HashSet<ApprovalId> = waiting.iter().map(|a| a.id).collect();
    let mut asked: HashSet<ApprovalId> = HashSet::new();
    sub.call(Request::Subscribe { thread }).await?;
    // Once idle, keep reading until every finished run has its chat
    // message, since those arrive after the run backlog.
    let mut followups = Followups::default();
    let mut idle_since: Option<tokio::time::Instant> = None;
    loop {
        let deadline = idle_since.map(|since| {
            if followups.complete() {
                tokio::time::Instant::now() + QUIET
            } else {
                since + IDLE_FALLBACK
            }
        });
        let next = match deadline {
            Some(d) => match tokio::time::timeout_at(d, sub.next()).await {
                Ok(next) => next?,
                Err(_) => return Ok(()),
            },
            None => sub.next().await?,
        };
        let Some(msg) = next else {
            bail!("crewd closed the connection.");
        };
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
                        RunStatus::Done | RunStatus::Failed => {
                            followups.expected.insert(run);
                        }
                        // A run stopped before it started gets no message
                        // of its own, only the thread-wide one.
                        RunStatus::Stopped if started(&mut ctl, run).await? => {
                            followups.expected.insert(run);
                        }
                        _ => {}
                    }
                    if until_idle {
                        update_idle(&mut ctl, thread, &pending, &mut idle_since).await?;
                    }
                } else {
                    idle_since = None;
                }
            }
            ServerMsg::Message { message, .. } => {
                if let Some(run) = message.run {
                    followups.seen.insert(run);
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
                    pending.insert(approval.id);
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
                    pending.remove(&approval.id);
                    println!(
                        "{}",
                        match decided {
                            ApprovalStatus::Approved => "[✓] approved",
                            ApprovalStatus::Declined => "[✗] declined",
                            _ => "[■] cancelled",
                        }
                    );
                    if until_idle {
                        update_idle(&mut ctl, thread, &pending, &mut idle_since).await?;
                    }
                }
            },
            ServerMsg::Response { .. } => {}
        }
    }
}

/// Starts or cancels the idle countdown after something changed.
async fn update_idle(
    ctl: &mut Client,
    thread: ThreadId,
    pending: &HashSet<ApprovalId>,
    idle_since: &mut Option<tokio::time::Instant>,
) -> Result<()> {
    if settled(ctl, thread, pending).await? {
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
                bail!(
                    "Discard removes the worktree and its branch, changes included. Add --yes to confirm."
                );
            }
            let thread = resolve_thread(&mut c, &thread).await?;
            c.call(Request::Discard { thread }).await?;
            Ok(())
        }
        Command::Agent(AgentCommand::Add(a)) => {
            let spec = AgentSpec {
                name: a.name,
                role: a.role,
                runtime: Runtime::ClaudeCode,
                model: a.model,
                instructions: a.instructions,
                permission_mode: if a.edit {
                    PermissionMode::EditFiles
                } else {
                    PermissionMode::ReadOnly
                },
                allowed_tools: vec![],
                handoff_policy: a.policy.map(|p| match p {
                    PolicyArg::Ask => HandoffPolicy::AskFirst,
                    PolicyArg::Budget => HandoffPolicy::Budget,
                    PolicyArg::PerAgent => HandoffPolicy::PerAgent,
                }),
                handoff_budget: a.budget,
                max_run_minutes: a.max_minutes,
            };
            let _: AgentId = c.ask(Request::CreateAgent { spec }).await?;
            Ok(())
        }
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
        };
        assert_eq!(event_line(&denied).as_deref(), Some("denied Bash"));

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
