//! The `crew` command line client for crewd.

mod client;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use client::{Client, NOT_RUNNING};
use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::{Event, RunStatus, ToolKind};
use crew_core::ids::{AgentId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::store::{Author, StoredRun};
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
        #[arg(value_parser = parse_id::<ThreadId>)]
        thread: ThreadId,
        #[arg(required = true, num_args = 1..)]
        body: Vec<String>,
    },
    /// Print a thread's history, then follow it live.
    Watch {
        #[arg(value_parser = parse_id::<ThreadId>)]
        thread: ThreadId,
        /// Exit once no run in the thread is queued or working.
        #[arg(long)]
        until_idle: bool,
    },
    /// List queued and working runs.
    Status,
    /// Stop one run, all runs of a thread, or everything.
    Stop(StopArgs),
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("target").required(true)))]
struct StopArgs {
    /// The run to stop.
    #[arg(group = "target", value_parser = parse_id::<RunId>)]
    run: Option<RunId>,
    /// Stop every run of this thread.
    #[arg(long, group = "target", value_parser = parse_id::<ThreadId>)]
    thread: Option<ThreadId>,
    /// Stop every run.
    #[arg(long, group = "target")]
    all: bool,
}

/// Parses an id, which is a plain uuid string on the wire.
fn parse_id<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| format!("`{s}` is not a valid id."))
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
        Event::Thinking { .. } => Some("thinking…".into()),
        Event::Message { text } => Some(format!("says: {}", first_line(text))),
        _ => None,
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

/// How long `--until-idle` waits for more output before it exits.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

async fn watch(home: &std::path::Path, thread: ThreadId, until_idle: bool) -> Result<()> {
    let mut sub = Client::connect(home).await?;
    // A second connection answers lookups so the stream stays in order.
    let mut ctl = Client::connect(home).await?;
    let mut names = Names::load(&mut ctl).await?;
    // The reason a run failed, taken from its last error or failed turn.
    let mut reasons: HashMap<RunId, String> = HashMap::new();
    sub.call(Request::Subscribe { thread }).await?;
    // Once idle, keep reading until the stream goes quiet so the chat
    // messages that follow the run backlog are printed too.
    let mut settling = false;
    loop {
        let next = if settling {
            match tokio::time::timeout(SETTLE, sub.next()).await {
                Ok(next) => next?,
                Err(_) => return Ok(()),
            }
        } else {
            sub.next().await?
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
                    settling = until_idle && idle(&mut ctl, thread).await?;
                } else {
                    settling = false;
                }
            }
            ServerMsg::Message { message, .. } => {
                let author = match message.author {
                    Author::You => "you".to_string(),
                    Author::System => "system".to_string(),
                    Author::Agent(a) => names.of_agent(&mut ctl, a).await?,
                };
                println!("{author}: {}", message.body);
            }
            ServerMsg::Response { .. } => {}
        }
    }
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
            let _: crew_core::ids::MessageId = c
                .ask(Request::PostMessage {
                    thread,
                    body: body.join(" "),
                })
                .await?;
            Ok(())
        }
        Command::Watch { thread, until_idle } => {
            drop(c);
            watch(&home, thread, until_idle).await
        }
        Command::Status => status(&mut c).await,
        Command::Stop(args) => {
            let req = match (args.run, args.thread) {
                (Some(run), _) => Request::StopRun { run },
                (None, Some(thread)) => Request::StopThread { thread },
                (None, None) => Request::StopAll,
            };
            c.call(req).await?;
            Ok(())
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
            let msg = format!("{e:#}");
            if msg == NOT_RUNNING {
                eprintln!("{msg}");
            } else {
                eprintln!("crew: {msg}");
            }
            ExitCode::FAILURE
        }
    }
}
