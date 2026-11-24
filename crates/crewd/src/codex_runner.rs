//! Runs one Codex agent turn through its own `codex app-server`.
//!
//! The process gets the run's MCP server on its command line and the
//! agent's settings in `thread/start` and `turn/start`. Nothing is written
//! to Codex's own config, and its sign-in is only checked with
//! `codex login status`.

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
    ApprovalPolicy, FileChange, InitializeParams, ItemNotification, SandboxMode,
    ServerNotification, ThreadItem, ThreadResumeParams, ThreadStartParams, ThreadStartResponse,
    TurnInterruptParams, TurnStartParams, TurnStartResponse, UserInput, method,
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

use crate::codex_asks::{Answer, AskFor, CREW_SERVER, Mapped, ReplyKind, Scope, map_request};
use crate::runner::{
    EVENT_BUFFER, PidSlot, READERS_GRACE, RunHandle, RunOutcome, STDERR_TAIL, escalate, join_until,
    read_tail, signal_group,
};
use crate::runtimes::codex_signed_in;

/// Shown when the `codex` binary can't be found.
pub const MISSING_CODEX: &str = "Couldn't start `codex`. Is Codex installed?";
/// Shown when `codex login status` says nobody is signed in.
pub const CODEX_SIGNED_OUT: &str = "Codex isn't signed in. Run `codex login`.";

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
/// The only crew-mcp tool a Codex agent gets. Codex asks through its own
/// app-server, so it has no use for `approve`.
const CODEX_CREW_TOOLS: &str = "handoff";

/// Puts a question to you as a Crew permission approval: the tool, its
/// input and the Codex item it is about. The receiver gets the answer.
/// An error means it couldn't be asked.
pub type Ask = Box<
    dyn Fn(&str, Value, Option<String>) -> Result<oneshot::Receiver<PermissionAnswer>, String>
        + Send
        + Sync,
>;

/// Posts a System note about the run in its thread.
pub type Note = Box<dyn Fn(&str) + Send + Sync>;

/// How a Codex run reaches the rest of Crew.
pub struct Hooks {
    pub ask: Ask,
    pub note: Note,
}

/// A Codex thread to continue.
#[derive(Debug, Clone, PartialEq)]
pub struct Resume {
    pub thread: String,
    /// What the agent missed, then the new prompt.
    pub prompt: String,
}

/// What one Codex run needs.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexInvocation {
    pub bin: PathBuf,
    /// The agent's name, for notes in the thread.
    pub agent: String,
    /// The thread's folder. The app-server runs here, and an `EditFiles`
    /// agent may write only here.
    pub cwd: PathBuf,
    pub model: String,
    pub permission_mode: PermissionMode,
    /// The Crew block and the agent's own instructions.
    pub developer_instructions: String,
    /// The prompt for a new thread: the chat so far, then the new prompt.
    pub prompt: String,
    /// The thread to continue, if any. If Codex no longer has it, a new
    /// thread starts with `prompt`.
    pub resume: Option<Resume>,
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
            let run_args = [
                String::from("--run"),
                mcp.run.to_string(),
                "--tools".into(),
                CODEX_CREW_TOOLS.into(),
            ];
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

    fn cwd_text(&self) -> String {
        self.cwd.to_string_lossy().into_owned()
    }

    fn sandbox(&self) -> SandboxMode {
        match self.permission_mode {
            PermissionMode::ReadOnly => SandboxMode::ReadOnly,
            PermissionMode::EditFiles | PermissionMode::Auto => SandboxMode::WorkspaceWrite,
        }
    }

    /// When Codex asks. `EditFiles` asks before every command that isn't a
    /// known safe read; `Auto` and `ReadOnly` only when the sandbox is in
    /// the way.
    fn approval_policy(&self) -> ApprovalPolicy {
        match self.permission_mode {
            PermissionMode::EditFiles => ApprovalPolicy::Untrusted,
            PermissionMode::ReadOnly | PermissionMode::Auto => ApprovalPolicy::OnRequest,
        }
    }

    /// The sandbox in `turn/start`'s documented object form. An
    /// `EditFiles` agent may write in its folder and nowhere else.
    pub fn sandbox_policy(&self) -> Value {
        match self.permission_mode {
            PermissionMode::ReadOnly => json!({"type": "readOnly", "networkAccess": false}),
            PermissionMode::EditFiles | PermissionMode::Auto => json!({
                "type": "workspaceWrite",
                "writableRoots": [self.cwd_text()],
                "networkAccess": false,
                // Temp folders stay writable, as by Codex's default:
                // compilers and test tools need them.
                "excludeTmpdirEnvVar": false,
                "excludeSlashTmp": false,
            }),
        }
    }

    /// The same writable root as config, for the thread itself.
    fn config(&self) -> Option<Value> {
        self.permission_mode
            .edits()
            .then(|| json!({"sandbox_workspace_write": {"writable_roots": [self.cwd_text()]}}))
    }

    fn model(&self) -> Option<String> {
        Some(self.model.clone()).filter(|m| !m.trim().is_empty())
    }

    fn instructions(&self) -> Option<String> {
        Some(self.developer_instructions.clone()).filter(|i| !i.is_empty())
    }

    pub fn start_params(&self) -> ThreadStartParams {
        ThreadStartParams {
            cwd: Some(self.cwd_text()),
            model: self.model(),
            approval_policy: Some(self.approval_policy()),
            sandbox: Some(self.sandbox()),
            developer_instructions: self.instructions(),
            config: self.config(),
        }
    }

    /// Everything is passed again, as the agent's settings and team may
    /// have changed since the thread started.
    pub fn resume_params(&self, thread: &str) -> ThreadResumeParams {
        ThreadResumeParams {
            thread_id: thread.to_string(),
            cwd: Some(self.cwd_text()),
            model: self.model(),
            approval_policy: Some(self.approval_policy()),
            sandbox: Some(self.sandbox()),
            developer_instructions: self.instructions(),
            config: self.config(),
            exclude_turns: Some(true),
        }
    }

    pub fn turn_params(&self, thread: &str, prompt: &str) -> TurnStartParams {
        TurnStartParams {
            thread_id: thread.to_string(),
            input: vec![UserInput::text(prompt)],
            effort: None,
            sandbox_policy: Some(self.sandbox_policy()),
        }
    }
}

/// Starts a Codex run and streams its events.
///
/// Must be called inside a tokio runtime. Dropping the returned
/// [`RunHandle`] stops the run. Its pid is set once the process starts,
/// after the sign-in check.
pub fn spawn_codex_run(
    inv: CodexInvocation,
    hooks: Hooks,
) -> (RunHandle, mpsc::Receiver<Event>, JoinHandle<RunOutcome>) {
    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    let (handle, stop_rx, pid) = RunHandle::starting();
    let join = tokio::spawn(run(inv, hooks, tx, stop_rx, pid));
    (handle, rx, join)
}

async fn run(
    inv: CodexInvocation,
    hooks: Hooks,
    tx: mpsc::Sender<Event>,
    mut stop_rx: watch::Receiver<bool>,
    pid: PidSlot,
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
    if let Some(id) = child.id() {
        let _ = pid.set(id);
    }
    let pgid = child.id().map(|p| Pid::from_raw(p as i32));
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
    let stderr_task = tokio::spawn(read_tail(stderr, tail.clone()));
    let (conn, incoming) = CodexConn::from_io(stdout, stdin);

    // The connection is gone once this returns, which closes stdin.
    let driven = Driver::new(&inv, &hooks, conn, &tx)
        .drive(incoming, &mut child, &mut stop_rx)
        .await;
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

/// Whether Codex refused a resume because it doesn't have the thread.
fn thread_missing(e: &CodexError) -> bool {
    matches!(e, CodexError::Rpc { message, .. } if message.to_lowercase().contains("not found"))
}

/// Starts the thread, or resumes it, and the turn. A thread Codex no
/// longer has is started again, once.
async fn start_turn(conn: &CodexConn, inv: &CodexInvocation) -> Result<Started, CodexError> {
    conn.initialize(&InitializeParams::crew(env!("CARGO_PKG_VERSION")))
        .await?;
    let resumed = match &inv.resume {
        Some(resume) => {
            let params = inv.resume_params(&resume.thread);
            match conn
                .call::<_, ThreadStartResponse>(method::THREAD_RESUME, &params)
                .await
            {
                Ok(opened) => Some((opened, resume.prompt.as_str())),
                Err(e) if thread_missing(&e) => {
                    crate::service::log(format!(
                        "Codex no longer has thread {}, starting a new one",
                        resume.thread
                    ));
                    None
                }
                Err(e) => return Err(e),
            }
        }
        None => None,
    };
    let (opened, prompt) = match resumed {
        Some(resumed) => resumed,
        None => {
            let opened: ThreadStartResponse =
                conn.call(method::THREAD_START, &inv.start_params()).await?;
            (opened, inv.prompt.as_str())
        }
    };
    let thread = opened.thread.id;
    let turn: TurnStartResponse = conn
        .call(method::TURN_START, &inv.turn_params(&thread, prompt))
        .await?;
    Ok(Started {
        thread,
        turn: turn.turn.id,
        model: Some(opened.model)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| inv.model.clone()),
        cwd: opened.thread.cwd.unwrap_or_else(|| inv.cwd_text()),
    })
}

/// Talks to one app-server for one turn.
struct Driver<'a> {
    inv: &'a CodexInvocation,
    hooks: &'a Hooks,
    conn: CodexConn,
    tx: &'a mpsc::Sender<Event>,
    /// False once nobody listens for events.
    listening: bool,
    translator: CodexTranslator,
    /// Requests put to you and not answered yet.
    asked: Vec<(RequestId, ReplyKind)>,
    /// The changes of each file change item still running, by item id.
    files: HashMap<String, Vec<FileChange>>,
}

impl<'a> Driver<'a> {
    fn new(
        inv: &'a CodexInvocation,
        hooks: &'a Hooks,
        conn: CodexConn,
        tx: &'a mpsc::Sender<Event>,
    ) -> Self {
        Driver {
            inv,
            hooks,
            conn,
            tx,
            listening: true,
            translator: CodexTranslator::new(),
            asked: Vec::new(),
            files: HashMap::new(),
        }
    }

    async fn send_all(&mut self, events: Vec<Event>) {
        for event in events {
            if self.listening && self.tx.send(event).await.is_err() {
                self.listening = false;
            }
        }
    }

    /// Runs until the turn ends, the run is stopped or the app-server
    /// goes away. Consumes the driver, so the connection closes on return.
    async fn drive(
        mut self,
        mut incoming: mpsc::UnboundedReceiver<Incoming>,
        child: &mut Child,
        stop_rx: &mut watch::Receiver<bool>,
    ) -> Driven {
        let driven = |end, stopping, exited| Driven {
            end,
            stopping,
            exited,
        };
        let started = tokio::select! {
            started = start_turn(&self.conn, self.inv) => started,
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
        let session =
            self.translator
                .session_started(&started.thread, &started.model, &started.cwd);
        self.send_all(session).await;

        // Answers you give later come back here.
        let (replies_tx, mut replies_rx) = mpsc::unbounded_channel::<(RequestId, Answer)>();
        // For those answers, which check whether the run is stopping.
        let stop_seen = stop_rx.clone();
        let mut stopping = false;
        let mut interrupt: Option<Pin<Box<dyn Future<Output = ()> + Send>>> = None;
        let mut stop_deadline: Option<Instant> = None;
        let mut exited: Option<ExitStatus> = None;
        let mut gone_deadline: Option<Instant> = None;
        let mut turn_error: Option<String> = None;

        let end = loop {
            tokio::select! {
                msg = incoming.recv() => {
                    let Some(msg) = msg else { break End::Gone };
                    let n = match msg {
                        Incoming::ServerRequest { id, method, params } => {
                            self.on_request(id, &method, &params, &replies_tx, &stop_seen).await;
                            continue;
                        }
                        Incoming::Notification(n) => n,
                    };
                    match &n {
                        ServerNotification::ItemStarted(ItemNotification {
                            item: ThreadItem::FileChange(f),
                            ..
                        }) => {
                            self.files.insert(f.id.clone(), f.changes.clone());
                        }
                        ServerNotification::ItemCompleted(ItemNotification {
                            item: ThreadItem::FileChange(f),
                            ..
                        }) => {
                            self.files.remove(&f.id);
                        }
                        _ => {}
                    }
                    let turn_over = matches!(n, ServerNotification::TurnCompleted(_));
                    if turn_over {
                        // An interrupt leaves running items open.
                        let reason = if stopping { STOPPED } else { UNFINISHED };
                        let closed = self.translator.close_open(reason);
                        self.send_all(closed).await;
                    }
                    let events = self.translator.on(&Incoming::Notification(n));
                    let mut ok = false;
                    for event in &events {
                        match event {
                            Event::TurnEnd { ok: o, .. } => ok = *o,
                            Event::Error { message } => turn_error = Some(message.clone()),
                            _ => {}
                        }
                    }
                    self.send_all(events).await;
                    if turn_over {
                        break End::Turn { ok, error: turn_error.take() };
                    }
                }
                Some((id, answer)) = replies_rx.recv() => {
                    self.answer(id, answer).await;
                }
                // A dropped handle stops the run too.
                stop = async { stop_rx.changed().await.is_err() || *stop_rx.borrow_and_update() }, if !stopping => {
                    if !stop {
                        continue;
                    }
                    stopping = true;
                    // Waiting asks first, so the interrupt finds nothing open.
                    for (id, _) in self.asked.clone() {
                        self.answer(id, Answer::Cancel).await;
                    }
                    stop_deadline = Some(Instant::now() + INTERRUPT_WAIT);
                    let (c, params) = (self.conn.clone(), TurnInterruptParams {
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
            let closed = self.translator.close_open(reason);
            self.send_all(closed).await;
        }
        driven(end, stopping, exited)
    }

    /// Answers a request at once, or asks you and answers once you decide.
    async fn on_request(
        &mut self,
        id: RequestId,
        method: &str,
        params: &Value,
        replies: &mpsc::UnboundedSender<(RequestId, Answer)>,
        stop: &watch::Receiver<bool>,
    ) {
        // The app-server may be gone; its end shows elsewhere.
        let scope = Scope {
            files: &self.files,
            crew_wired: self.inv.mcp.is_some(),
            cwd: &self.inv.cwd,
            read_only: self.inv.permission_mode == PermissionMode::ReadOnly,
            agent: &self.inv.agent,
        };
        let AskFor {
            kind,
            tool,
            input,
            item,
        } = match map_request(method, params, &scope) {
            Mapped::Reply(result) => {
                let _ = self.conn.reply(id, result).await;
                return;
            }
            Mapped::Declined { reply, note } => {
                let _ = self.conn.reply(id, reply).await;
                (self.hooks.note)(&note);
                return;
            }
            Mapped::Refuse(msg) => {
                let _ = self.conn.reply_error(id, NOT_HANDLED, &msg).await;
                return;
            }
            Mapped::Ask(ask) => ask,
        };
        match (self.hooks.ask)(&tool, input, item) {
            Ok(answer) => {
                self.asked.push((id.clone(), kind));
                let (replies, stop) = (replies.clone(), stop.clone());
                tokio::spawn(async move {
                    let answer = answer.await;
                    // Crew's answer to a stopping run is a cancel.
                    let stopping = *stop.borrow() || stop.has_changed().is_err();
                    let answer = match answer {
                        Ok(a) if !stopping && a.allow => Answer::Yes,
                        Ok(_) if !stopping => Answer::No,
                        _ => Answer::Cancel,
                    };
                    let _ = replies.send((id, answer));
                });
            }
            Err(e) => {
                crate::service::log(format!("couldn't ask about {tool}: {e}"));
                let _ = self.conn.reply(id, kind.reply(Answer::No)).await;
            }
        }
    }

    /// Replies to a request put to you, once. Later answers to it, as
    /// after a stop already cancelled it, are dropped.
    async fn answer(&mut self, id: RequestId, answer: Answer) {
        let Some(pos) = self.asked.iter().position(|(asked, _)| *asked == id) else {
            return;
        };
        let (id, kind) = self.asked.remove(pos);
        let _ = self.conn.reply(id, kind.reply(answer)).await;
    }
}

/// Waits until `deadline`, or for ever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
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
            agent: "Ada".into(),
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
        let turn =
            serde_json::to_value(inv(PermissionMode::ReadOnly).turn_params("thr", "go")).unwrap();
        assert_eq!(
            turn["sandboxPolicy"],
            json!({"type": "readOnly", "networkAccess": false})
        );
        let edit = serde_json::to_value(inv(PermissionMode::EditFiles).start_params()).unwrap();
        assert_eq!(edit["sandbox"], "workspace-write");
        assert_eq!(edit["approvalPolicy"], "untrusted");
        assert_eq!(
            edit["config"],
            json!({"sandbox_workspace_write": {"writable_roots": ["/work/demo"]}})
        );
        let turn =
            serde_json::to_value(inv(PermissionMode::EditFiles).turn_params("thr", "go")).unwrap();
        assert_eq!(
            turn["sandboxPolicy"]["writableRoots"],
            json!(["/work/demo"])
        );
        let resume =
            serde_json::to_value(inv(PermissionMode::EditFiles).resume_params("thr")).unwrap();
        assert_eq!(resume["threadId"], "thr");
        assert_eq!(resume["sandbox"], "workspace-write");
        assert_eq!(resume["approvalPolicy"], "untrusted");
        assert_eq!(resume["excludeTurns"], true);
        assert_eq!(resume["developerInstructions"], "Be brief.");
    }

    #[test]
    fn auto_writes_in_the_folder_and_asks_only_on_request() {
        let auto = inv(PermissionMode::Auto);
        let start = serde_json::to_value(auto.start_params()).unwrap();
        assert_eq!(start["sandbox"], "workspace-write");
        assert_eq!(start["approvalPolicy"], "on-request");
        assert_eq!(
            start["config"],
            json!({"sandbox_workspace_write": {"writable_roots": ["/work/demo"]}})
        );
        let resume = serde_json::to_value(auto.resume_params("thr")).unwrap();
        assert_eq!(resume["sandbox"], "workspace-write");
        assert_eq!(resume["approvalPolicy"], "on-request");
        let turn = serde_json::to_value(auto.turn_params("thr", "go")).unwrap();
        assert_eq!(turn["sandboxPolicy"]["type"], "workspaceWrite");
        let read =
            serde_json::to_value(inv(PermissionMode::ReadOnly).resume_params("thr")).unwrap();
        assert_eq!(read["approvalPolicy"], "on-request");
    }

    #[test]
    fn crew_mcp_is_passed_as_toml_settings_with_handoff_only() {
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
                r#"mcp_servers.crew_run.command="/work/bin/crew \"mcp\"""#.into(),
                "-c".into(),
                format!(r#"mcp_servers.crew_run.args=["--run","{run}","--tools","handoff"]"#),
                "-c".into(),
                r#"mcp_servers.crew_run.env.CREW_HOME="/work/home""#.into(),
            ]
        );
    }

    #[test]
    fn only_a_missing_thread_starts_a_new_one() {
        let rpc = |message: &str| CodexError::Rpc {
            code: -32600,
            message: message.into(),
            data: None,
        };
        assert!(thread_missing(&rpc("thread not found: abc")));
        assert!(thread_missing(&rpc("Thread Not Found")));
        assert!(!thread_missing(&rpc("model is not available")));
        assert!(!thread_missing(&CodexError::Closed));
    }

    #[test]
    fn colour_codes_are_dropped() {
        assert_eq!(
            plain("\u{1b}[2m2026\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m boom"),
            "2026 ERROR boom"
        );
    }
}
