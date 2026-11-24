//! Runs one Codex agent turn through its own `codex app-server`.
//!
//! The process gets the run's MCP server on its command line and the
//! agent's settings in `thread/start`. Nothing is written to Codex's own
//! config, and its sign-in is only checked with `codex login status`.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crew_core::agent::PermissionMode;
use crew_core::claude::McpWiring;
use crew_core::codex::proto::{
    ApprovalDecision, ApprovalPolicy, ApprovalResponse, ElicitationParams, ElicitationResponse,
    FileChange, InitializeParams, ItemNotification, SandboxMode, ServerNotification, ServerRequest,
    ThreadItem, ThreadResumeParams, ThreadStartParams, ThreadStartResponse, TurnInterruptParams,
    TurnStartParams, TurnStartResponse, UserInput, method,
};
use crew_core::codex::{CodexConn, CodexError, CodexTranslator, Incoming, RequestId};
use crew_core::event::Event;
use crew_proto::PermissionAnswer;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::runner::{
    EVENT_BUFFER, READERS_GRACE, RunHandle, RunOutcome, STDERR_TAIL, escalate, join_until,
    read_tail, signal_group,
};
use crate::runtimes::codex_signed_in;

/// Shown when the `codex` binary can't be found.
pub const MISSING_CODEX: &str = "Couldn't start `codex`. Is Codex installed?";
/// Shown when `codex login status` says nobody is signed in.
pub const CODEX_SIGNED_OUT: &str = "Codex isn't signed in. Run `codex login`.";

/// The MCP server crew-mcp is registered as. Its asks pass without you.
const CREW_SERVER: &str = "crew";
/// How long a stopped turn gets to end after `turn/interrupt`.
const INTERRUPT_WAIT: Duration = Duration::from_secs(3);
/// How long the app-server gets to exit once its stdin is closed.
const EXIT_WAIT: Duration = Duration::from_secs(2);
/// JSON-RPC "method not found", for server requests Crew doesn't handle.
const NOT_HANDLED: i64 = -32601;
/// Tool results of items still open when a stopped run ends.
const STOPPED: &str = "Stopped.";
/// Tool results of items still open when a run fails.
const UNFINISHED: &str = "Didn't finish.";

/// Puts a question to you as a Crew permission approval: the tool, its
/// input and the Codex item it is about. The receiver gets the answer.
/// An error means it couldn't be asked.
pub type Ask = Box<
    dyn Fn(&str, Value, Option<String>) -> Result<oneshot::Receiver<PermissionAnswer>, String>
        + Send
        + Sync,
>;

/// What one Codex run needs.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexInvocation {
    pub bin: PathBuf,
    /// The thread's folder. The app-server runs here, and an `EditFiles`
    /// agent may write only here.
    pub cwd: PathBuf,
    pub model: String,
    pub permission_mode: PermissionMode,
    /// The Crew block and the agent's own instructions.
    pub developer_instructions: String,
    pub prompt: String,
    /// The Codex thread to continue, if any.
    pub resume: Option<String>,
    pub mcp: Option<McpWiring>,
}

impl CodexInvocation {
    /// Arguments of `codex`: the app-server, with crew-mcp as an MCP
    /// server for this process only.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["app-server".to_string()];
        if let Some(mcp) = &self.mcp {
            // JSON strings and arrays of strings are valid TOML values.
            let text = |v: &str| serde_json::to_string(v).expect("a string serializes");
            let run_args = [String::from("--run"), mcp.run.to_string()];
            for setting in [
                format!(
                    "mcp_servers.{CREW_SERVER}.command={}",
                    text(&mcp.crew_mcp_bin.to_string_lossy())
                ),
                format!(
                    "mcp_servers.{CREW_SERVER}.args={}",
                    serde_json::to_string(&run_args).expect("strings serialize")
                ),
                format!(
                    "mcp_servers.{CREW_SERVER}.env.CREW_HOME={}",
                    text(&mcp.crew_home.to_string_lossy())
                ),
            ] {
                args.push("-c".into());
                args.push(setting);
            }
        }
        args
    }

    fn sandbox(&self) -> SandboxMode {
        match self.permission_mode {
            PermissionMode::ReadOnly => SandboxMode::ReadOnly,
            PermissionMode::EditFiles => SandboxMode::WorkspaceWrite,
        }
    }

    /// An `EditFiles` agent may write in its folder and nowhere else.
    fn config(&self) -> Option<Value> {
        (self.permission_mode == PermissionMode::EditFiles).then(
            || json!({"sandbox_workspace_write": {"writable_roots": [self.cwd.to_string_lossy()]}}),
        )
    }

    fn model(&self) -> Option<String> {
        Some(self.model.clone()).filter(|m| !m.trim().is_empty())
    }

    pub fn start_params(&self) -> ThreadStartParams {
        ThreadStartParams {
            cwd: Some(self.cwd.to_string_lossy().into_owned()),
            model: self.model(),
            approval_policy: Some(ApprovalPolicy::OnRequest),
            sandbox: Some(self.sandbox()),
            developer_instructions: Some(self.developer_instructions.clone())
                .filter(|i| !i.is_empty()),
            config: self.config(),
        }
    }

    /// Codex keeps the developer instructions with the thread. The rest
    /// is passed again, as the agent's settings may have changed.
    pub fn resume_params(&self, thread: &str) -> ThreadResumeParams {
        ThreadResumeParams {
            thread_id: thread.to_string(),
            cwd: Some(self.cwd.to_string_lossy().into_owned()),
            model: self.model(),
            approval_policy: Some(ApprovalPolicy::OnRequest),
            sandbox: Some(self.sandbox()),
            developer_instructions: None,
            config: self.config(),
            exclude_turns: Some(true),
        }
    }
}

/// Starts a Codex run and streams its events.
///
/// Must be called inside a tokio runtime. Dropping the returned
/// [`RunHandle`] stops the run. Its pid is unknown: the process starts
/// after the sign-in check.
pub fn spawn_codex_run(
    inv: CodexInvocation,
    ask: Ask,
) -> (RunHandle, mpsc::Receiver<Event>, JoinHandle<RunOutcome>) {
    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    let (handle, stop_rx) = RunHandle::idle();
    let join = tokio::spawn(run(inv, ask, tx, stop_rx));
    (handle, rx, join)
}

async fn run(
    inv: CodexInvocation,
    ask: Ask,
    tx: mpsc::Sender<Event>,
    mut stop_rx: watch::Receiver<bool>,
) -> RunOutcome {
    if !inv.cwd.is_dir() {
        return RunOutcome::Failed(format!(
            "Working folder {} doesn't exist.",
            inv.cwd.display()
        ));
    }
    let signed_in = tokio::select! {
        signed_in = codex_signed_in(&inv.bin) => signed_in,
        // A stop, or the handle being dropped.
        _ = stop_rx.wait_for(|stop| *stop) => return RunOutcome::Stopped,
    };
    match signed_in {
        Err(e) => return RunOutcome::Failed(start_error(&e)),
        Ok(Some(false)) => return RunOutcome::Failed(CODEX_SIGNED_OUT.into()),
        // Unknown: let the app-server say what is wrong.
        Ok(_) => {}
    }

    let spawned = Command::new(&inv.bin)
        .args(inv.args())
        .current_dir(&inv.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return RunOutcome::Failed(start_error(&e)),
    };
    let pgid = child.id().map(|p| Pid::from_raw(p as i32));
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
    let stderr_task = tokio::spawn(read_tail(stderr, tail.clone()));
    let (conn, incoming) = CodexConn::from_io(stdout, stdin);

    // The connection is gone once this returns, which closes stdin.
    let driven = drive(&inv, &ask, conn, incoming, &mut child, &mut stop_rx, &tx).await;
    drop(tx);

    let mut status = driven.exited;
    if status.is_none() {
        // A turn that ended gets to exit on its own. A stopped one had
        // its chance with the interrupt.
        if !matches!(driven.end, End::Stopped) {
            status = tokio::time::timeout(EXIT_WAIT, child.wait())
                .await
                .ok()
                .and_then(Result::ok);
        }
        if status.is_none() {
            escalate(&mut child, pgid).await;
        }
    }
    // Clear anything the app-server left behind in its group.
    signal_group(pgid, Signal::SIGKILL);
    join_until(stderr_task, Instant::now() + READERS_GRACE).await;
    let tail: Vec<String> =
        std::mem::take(&mut *tail.lock().unwrap_or_else(PoisonError::into_inner))
            .into_iter()
            .map(|line| plain(&line))
            .filter(|line| !line.trim().is_empty())
            .collect();
    let tail = (!tail.is_empty()).then(|| tail.join("\n"));

    if driven.stopping {
        return RunOutcome::Stopped;
    }
    match driven.end {
        End::Turn { ok: true, .. } => RunOutcome::Done,
        End::Turn { ok: false, error } => RunOutcome::Failed(
            error
                .or(tail)
                .unwrap_or_else(|| "Codex ended the turn without finishing it.".into()),
        ),
        End::Refused(msg) => RunOutcome::Failed(match tail {
            Some(tail) => format!("{msg}\n{tail}"),
            None => msg,
        }),
        End::Gone => RunOutcome::Failed(tail.unwrap_or_else(|| exit_message(status))),
        End::Stopped => RunOutcome::Stopped,
    }
}

/// How the driving of a turn ended.
enum End {
    /// `turn/completed` arrived. `error` is the turn's own error.
    Turn { ok: bool, error: Option<String> },
    /// Stopped before the turn ended.
    Stopped,
    /// The app-server went away first.
    Gone,
    /// Codex refused to start the thread or the turn.
    Refused(String),
}

struct Driven {
    end: End,
    /// Whether a stop was asked for.
    stopping: bool,
    /// The app-server's exit, if it exited meanwhile.
    exited: Option<ExitStatus>,
}

/// The thread and turn a run works in.
struct Started {
    thread: String,
    turn: String,
    model: String,
    cwd: String,
}

/// Starts the thread (or resumes it) and the turn.
async fn start_turn(conn: &CodexConn, inv: &CodexInvocation) -> Result<Started, CodexError> {
    conn.initialize(&InitializeParams::crew(env!("CARGO_PKG_VERSION")))
        .await?;
    let opened: ThreadStartResponse = match &inv.resume {
        Some(thread) => {
            conn.call(method::THREAD_RESUME, &inv.resume_params(thread))
                .await?
        }
        None => conn.call(method::THREAD_START, &inv.start_params()).await?,
    };
    let thread = opened.thread.id;
    let params = TurnStartParams {
        thread_id: thread.clone(),
        input: vec![UserInput::text(inv.prompt.clone())],
        effort: None,
    };
    let turn: TurnStartResponse = conn.call(method::TURN_START, &params).await?;
    Ok(Started {
        thread,
        turn: turn.turn.id,
        model: Some(opened.model)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| inv.model.clone()),
        cwd: opened
            .thread
            .cwd
            .unwrap_or_else(|| inv.cwd.to_string_lossy().into_owned()),
    })
}

/// Sends events, dropping them once nobody listens.
struct Out<'a> {
    tx: &'a mpsc::Sender<Event>,
    alive: bool,
}

impl Out<'_> {
    async fn send_all(&mut self, events: Vec<Event>) {
        for event in events {
            if self.alive && self.tx.send(event).await.is_err() {
                self.alive = false;
            }
        }
    }
}

/// Talks to the app-server until the turn ends, the run is stopped or
/// the app-server goes away.
async fn drive(
    inv: &CodexInvocation,
    ask: &Ask,
    conn: CodexConn,
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    child: &mut Child,
    stop_rx: &mut watch::Receiver<bool>,
    tx: &mpsc::Sender<Event>,
) -> Driven {
    let mut out = Out { tx, alive: true };
    let mut translator = CodexTranslator::new();
    let driven = |end, stopping, exited| Driven {
        end,
        stopping,
        exited,
    };

    let started = tokio::select! {
        started = start_turn(&conn, inv) => started,
        _ = stop_rx.wait_for(|stop| *stop) => return driven(End::Stopped, true, None),
        status = child.wait() => return driven(End::Gone, false, status.ok()),
    };
    let started = match started {
        Ok(started) => started,
        Err(CodexError::Rpc { message, .. }) => {
            let msg = format!("Codex refused to start the turn: {message}");
            return driven(End::Refused(msg), false, None);
        }
        Err(CodexError::Json(e)) => {
            let msg = format!("Codex sent a reply Crew couldn't read: {e}");
            return driven(End::Refused(msg), false, None);
        }
        Err(_) => return driven(End::Gone, false, None),
    };
    out.send_all(translator.session_started(&started.thread, &started.model, &started.cwd))
        .await;

    let (replies_tx, mut replies_rx) = mpsc::unbounded_channel::<(RequestId, Value)>();
    let mut file_changes: HashMap<String, Vec<FileChange>> = HashMap::new();
    let mut stopping = false;
    let mut interrupt: Option<Pin<Box<dyn Future<Output = ()> + Send>>> = None;
    let mut stop_deadline: Option<Instant> = None;
    let mut exited: Option<ExitStatus> = None;
    let mut gone_deadline: Option<Instant> = None;
    let mut turn_error: Option<String> = None;
    // For answers decided later, which check whether the run is stopping.
    let stop_seen = stop_rx.clone();

    let end = loop {
        tokio::select! {
            msg = incoming.recv() => {
                let Some(msg) = msg else { break End::Gone };
                let n = match msg {
                    Incoming::ServerRequest { id, method, params } => {
                        let asked = Asked { conn: &conn, ask, replies: &replies_tx, stop: &stop_seen, files: &file_changes };
                        asked.answer(id, &method, &params).await;
                        continue;
                    }
                    Incoming::Notification(n) => n,
                };
                if let ServerNotification::ItemStarted(ItemNotification {
                    item: ThreadItem::FileChange(f),
                    ..
                }) = &n
                {
                    file_changes.insert(f.id.clone(), f.changes.clone());
                }
                let turn_over = matches!(n, ServerNotification::TurnCompleted(_));
                if turn_over {
                    // An interrupt leaves running items open.
                    let reason = if stopping { STOPPED } else { UNFINISHED };
                    out.send_all(translator.close_open(reason)).await;
                }
                let events = translator.on(&Incoming::Notification(n));
                let mut ok = false;
                for event in &events {
                    match event {
                        Event::TurnEnd { ok: o, .. } => ok = *o,
                        Event::Error { message } => turn_error = Some(message.clone()),
                        _ => {}
                    }
                }
                out.send_all(events).await;
                if turn_over {
                    break End::Turn { ok, error: turn_error.take() };
                }
            }
            Some((id, result)) = replies_rx.recv() => {
                // The app-server may be gone; its end shows elsewhere.
                let _ = conn.reply(id, result).await;
            }
            // A dropped handle stops the run too.
            stop = async { stop_rx.changed().await.is_err() || *stop_rx.borrow_and_update() }, if !stopping => {
                if !stop {
                    continue;
                }
                stopping = true;
                stop_deadline = Some(Instant::now() + INTERRUPT_WAIT);
                let (c, params) = (conn.clone(), TurnInterruptParams {
                    thread_id: started.thread.clone(),
                    turn_id: started.turn.clone(),
                });
                interrupt = Some(Box::pin(async move {
                    // Only the turn ending matters, not this reply.
                    let _ = c.call::<_, Value>(method::TURN_INTERRUPT, &params).await;
                }));
            }
            _ = async { interrupt.as_mut().expect("guarded").await }, if interrupt.is_some() => {
                interrupt = None;
            }
            _ = sleep_until(stop_deadline) => break End::Stopped,
            status = child.wait(), if exited.is_none() => {
                exited = status.ok();
                // Its last lines may still be on the way.
                gone_deadline = Some(Instant::now() + READERS_GRACE);
            }
            _ = sleep_until(gone_deadline) => break End::Gone,
        }
    };
    if !matches!(end, End::Turn { .. }) {
        let reason = if stopping { STOPPED } else { UNFINISHED };
        out.send_all(translator.close_open(reason)).await;
    }
    driven(end, stopping, exited)
}

/// Waits until `deadline`, or for ever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// What a request from the app-server is answered with.
#[derive(Clone, Copy)]
enum ReplyKind {
    /// `{"decision": ...}` for a command or file change.
    Approval,
    /// `{"action": ...}` for an MCP elicitation.
    Elicitation,
}

#[derive(Clone, Copy)]
enum Answer {
    Yes,
    No,
    /// The run is stopping.
    Cancel,
}

impl ReplyKind {
    fn reply(self, answer: Answer) -> Value {
        let value = match self {
            ReplyKind::Approval => serde_json::to_value(ApprovalResponse::new(match answer {
                Answer::Yes => ApprovalDecision::Accept,
                Answer::No => ApprovalDecision::Decline,
                Answer::Cancel => ApprovalDecision::Cancel,
            })),
            ReplyKind::Elicitation => serde_json::to_value(match answer {
                Answer::Yes => ElicitationResponse::accept(),
                Answer::No => ElicitationResponse::decline(),
                Answer::Cancel => ElicitationResponse::cancel(),
            }),
        };
        value.expect("replies serialize")
    }
}

/// What answering one server request needs.
struct Asked<'a> {
    conn: &'a CodexConn,
    ask: &'a Ask,
    /// Answers decided later go back to the driving loop here.
    replies: &'a mpsc::UnboundedSender<(RequestId, Value)>,
    stop: &'a watch::Receiver<bool>,
    /// The changes of each file change item, by item id.
    files: &'a HashMap<String, Vec<FileChange>>,
}

impl Asked<'_> {
    /// Answers at once, or asks you and answers once you decide.
    async fn answer(&self, id: RequestId, method: &str, params: &Value) {
        let (kind, tool, input, item) = match ServerRequest::parse(method, params) {
            ServerRequest::CommandApproval(p) => {
                let input = json!({
                    "command": p.command.unwrap_or_default(),
                    "cwd": p.cwd.unwrap_or_default(),
                });
                (
                    ReplyKind::Approval,
                    "Bash".to_string(),
                    input,
                    Some(p.item_id),
                )
            }
            ServerRequest::FileChangeApproval(p) => {
                let changes = self.files.get(&p.item_id).map_or(&[][..], Vec::as_slice);
                let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
                let listed: Vec<Value> = changes
                    .iter()
                    .map(|c| json!({"path": c.path, "diff": c.diff}))
                    .collect();
                let input = json!({"path": paths.join(", "), "changes": listed});
                (
                    ReplyKind::Approval,
                    "Edit".to_string(),
                    input,
                    Some(p.item_id),
                )
            }
            ServerRequest::McpElicitation(p) if p.server_name == CREW_SERVER => {
                let reply = ReplyKind::Elicitation.reply(Answer::Yes);
                let _ = self.conn.reply(id, reply).await;
                return;
            }
            ServerRequest::McpElicitation(p) => {
                let input = p
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("tool_params"))
                    .cloned()
                    .unwrap_or_else(|| json!({"message": p.message}));
                (ReplyKind::Elicitation, elicited_tool(&p), input, None)
            }
            ServerRequest::Raw(_) => {
                let msg = format!("Crew doesn't handle {method}.");
                let _ = self.conn.reply_error(id, NOT_HANDLED, &msg).await;
                return;
            }
        };
        match (self.ask)(&tool, input, item) {
            Ok(answer) => {
                let (replies, stop) = (self.replies.clone(), self.stop.clone());
                tokio::spawn(async move {
                    let answer = answer.await;
                    // Crew's answer to a stopping run is a cancel, which
                    // also ends the turn.
                    let stopping = *stop.borrow() || stop.has_changed().is_err();
                    let answer = match answer {
                        Ok(a) if !stopping && a.allow => Answer::Yes,
                        Ok(_) if !stopping => Answer::No,
                        _ => Answer::Cancel,
                    };
                    let _ = replies.send((id, kind.reply(answer)));
                });
            }
            Err(e) => {
                crate::service::log(format!("couldn't ask about {tool}: {e}"));
                let _ = self.conn.reply(id, kind.reply(Answer::No)).await;
            }
        }
    }
}

/// `mcp__<server>__<tool>` for an MCP tool call ask, read from its
/// message (`... run tool "name"?`). Just the server if it names none.
fn elicited_tool(p: &ElicitationParams) -> String {
    let tool = p
        .message
        .split_once("tool \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty());
    match tool {
        Some(tool) => format!("mcp__{}__{tool}", p.server_name),
        None => format!("mcp__{}", p.server_name),
    }
}

fn start_error(e: &std::io::Error) -> String {
    if e.kind() == ErrorKind::NotFound {
        MISSING_CODEX.to_string()
    } else {
        format!("Couldn't start `codex`: {e}")
    }
}

fn exit_message(status: Option<ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.map(|s| (s.code(), s.signal())) {
        Some((Some(code), _)) => format!("`codex app-server` exited with code {code}."),
        Some((None, Some(sig))) => format!("`codex app-server` was ended by signal {sig}."),
        _ => "`codex app-server` stopped before the turn ended.".to_string(),
    }
}

/// A log line without its terminal colour codes.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // `ESC [ ... letter`.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_core::ids::RunId;

    fn inv(mode: PermissionMode) -> CodexInvocation {
        CodexInvocation {
            bin: "codex".into(),
            cwd: "/work/demo".into(),
            model: "gpt-test".into(),
            permission_mode: mode,
            developer_instructions: "Be brief.".into(),
            prompt: "go".into(),
            resume: None,
            mcp: None,
        }
    }

    #[test]
    fn permission_modes_map_to_sandbox_and_approvals() {
        let read = serde_json::to_value(inv(PermissionMode::ReadOnly).start_params()).unwrap();
        assert_eq!(
            read,
            json!({"cwd": "/work/demo", "model": "gpt-test", "approvalPolicy": "on-request",
                   "sandbox": "read-only", "developerInstructions": "Be brief."})
        );
        let edit = serde_json::to_value(inv(PermissionMode::EditFiles).start_params()).unwrap();
        assert_eq!(edit["sandbox"], "workspace-write");
        assert_eq!(edit["approvalPolicy"], "on-request");
        assert_eq!(
            edit["config"],
            json!({"sandbox_workspace_write": {"writable_roots": ["/work/demo"]}})
        );
        let resume =
            serde_json::to_value(inv(PermissionMode::EditFiles).resume_params("thr")).unwrap();
        assert_eq!(resume["threadId"], "thr");
        assert_eq!(resume["sandbox"], "workspace-write");
        assert_eq!(resume["excludeTurns"], true);
        assert!(resume.get("developerInstructions").is_none());
    }

    #[test]
    fn crew_mcp_is_passed_as_toml_settings() {
        let run = RunId::new();
        let mut i = inv(PermissionMode::ReadOnly);
        assert_eq!(i.args(), ["app-server"]);
        i.mcp = Some(McpWiring {
            crew_mcp_bin: "/work/bin/crew \"mcp\"".into(),
            run,
            crew_home: "/work/home".into(),
        });
        assert_eq!(
            i.args(),
            [
                "app-server".to_string(),
                "-c".into(),
                r#"mcp_servers.crew.command="/work/bin/crew \"mcp\"""#.into(),
                "-c".into(),
                format!(r#"mcp_servers.crew.args=["--run","{run}"]"#),
                "-c".into(),
                r#"mcp_servers.crew.env.CREW_HOME="/work/home""#.into(),
            ]
        );
    }

    #[test]
    fn elicited_tools_are_read_from_the_message() {
        let p = |server: &str, message: &str| ElicitationParams {
            server_name: server.into(),
            message: message.into(),
            ..Default::default()
        };
        assert_eq!(
            elicited_tool(&p(
                "github",
                "Allow the github MCP server to run tool \"create_issue\"?"
            )),
            "mcp__github__create_issue"
        );
        assert_eq!(elicited_tool(&p("docs", "Pick a file")), "mcp__docs");
    }

    #[test]
    fn colour_codes_are_dropped() {
        assert_eq!(
            plain("\u{1b}[2m2026\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m boom"),
            "2026 ERROR boom"
        );
    }
}
