//! Unix socket server: framing, handshake and the per-connection loop.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use crew_core::ids::{ApprovalId, RunId, ThreadId};
use crew_core::store::Store;
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, mpsc};

use crate::approvals::stopped_answer;
use crate::peer::{self, Peer};
use crate::service::{Backlog, CliBins, Notice, Reply, Service};

/// Socket file name inside the crew home.
pub const SOCKET_NAME: &str = "crewd.sock";
/// Lock file name inside the crew home.
pub const LOCK_NAME: &str = "crewd.lock";
/// Database file name inside the crew home.
pub const DB_NAME: &str = "crew.db";
/// Folder inside the crew home that holds thread worktrees.
pub const WORKTREES_DIR: &str = "worktrees";

/// Settings for [`serve`].
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Most runs that may work at once.
    pub max_runs: usize,
    /// Overrides the `claude` binary for every run.
    pub claude_bin: Option<PathBuf>,
    /// Overrides the `codex` binary for every Codex run and `Runtimes`.
    pub codex_bin: Option<PathBuf>,
    /// The crew-mcp binary passed to every run, so agents can hand off
    /// and ask for permission. Runs go without it if it is `None` or
    /// missing.
    pub crew_mcp_bin: Option<PathBuf>,
    /// How long one minute of an agent's `max_run_minutes` lasts. Tests
    /// make it short.
    pub minute: Duration,
    /// How many notifications a subscriber may fall behind before crewd
    /// sends it `Resync` and hangs up.
    pub notice_buffer: usize,
    /// How long `Runtimes` waits for slow CLIs before answering with what
    /// it found on `PATH`. `None` is one second.
    pub runtimes_wait: Option<Duration>,
    /// Claude Code's home, whose sessions are shown read-only as terminal
    /// sessions. `None` turns them off. The `crewd` binary uses
    /// [`claude_home_from`].
    pub claude_home: Option<PathBuf>,
    /// For tests only: extra time each change count of a channel overview
    /// takes, to make counts slow. Leave it at zero.
    pub count_delay: Duration,
}

/// Claude Code's home: `CREW_CLAUDE_HOME`, else `CLAUDE_CONFIG_DIR` (where
/// Claude Code itself keeps it when set), else `~/.claude`. Empty values
/// count as unset.
pub fn claude_home_from(
    crew_claude_home: Option<std::ffi::OsString>,
    claude_config_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let set = |v: Option<std::ffi::OsString>| v.filter(|v| !v.is_empty());
    set(crew_claude_home)
        .or_else(|| set(claude_config_dir))
        .map(PathBuf::from)
        .or_else(|| set(home).map(|h| PathBuf::from(h).join(".claude")))
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            max_runs: 4,
            claude_bin: None,
            codex_bin: None,
            crew_mcp_bin: default_crew_mcp_bin(),
            minute: Duration::from_secs(60),
            notice_buffer: 4096,
            runtimes_wait: None,
            count_delay: Duration::ZERO,
            claude_home: None,
        }
    }
}

/// `crew-mcp` next to the running executable.
pub fn default_crew_mcp_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("crew-mcp"))
}

/// The crew-mcp binary to wire into runs, if it is there.
fn usable_crew_mcp(bin: Option<PathBuf>) -> Option<PathBuf> {
    let bin = bin?;
    match std::fs::canonicalize(&bin) {
        Ok(path) if path.is_file() => Some(path),
        _ => {
            log(format!(
                "{} not found, agents run without handoffs and permission prompts",
                bin.display()
            ));
            None
        }
    }
}

fn log(msg: impl std::fmt::Display) {
    eprintln!("crewd: {msg}");
}

/// Serves clients on `home/crewd.sock` until SIGINT, SIGTERM or SIGHUP.
/// Then stops every run, marks them `Interrupted` and returns.
pub async fn serve(home: &Path, opts: ServeOptions) -> anyhow::Result<()> {
    let mut sigint = signal(SignalKind::interrupt()).context("handling SIGINT")?;
    let mut sigterm = signal(SignalKind::terminate()).context("handling SIGTERM")?;
    let mut sighup = signal(SignalKind::hangup()).context("handling SIGHUP")?;
    let signals = async move {
        tokio::select! {
            _ = sigint.recv() => log("got SIGINT"),
            _ = sigterm.recv() => log("got SIGTERM"),
            _ = sighup.recv() => log("got SIGHUP"),
        }
    };
    serve_until(home, opts, signals).await
}

/// Like [`serve`], but shuts down when `shutdown` completes instead of on
/// a signal.
pub async fn serve_until(
    home: &Path,
    opts: ServeOptions,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .with_context(|| format!("creating {}", home.display()))?;
    // Also tightens a home that already existed. The socket is bound in
    // here, so it is never reachable by others even before its chmod.
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting the mode of {}", home.display()))?;

    // Held for as long as this daemon runs. Taken before anything touches
    // the database, so a second crewd can't disturb the live one.
    let sock = home.join(SOCKET_NAME);
    if UnixStream::connect(&sock).await.is_ok() {
        return Err(AlreadyRunning(sock).into());
    }
    let _lock = lock_home(home, &sock)?;

    let store = Store::open(&home.join(DB_NAME)).context("opening the database")?;
    let interrupted = store.mark_unfinished_runs_interrupted()?;
    if interrupted > 0 {
        log(format!(
            "marked {interrupted} unfinished runs as interrupted"
        ));
    }

    // We hold the lock, so any socket left here is stale.
    if sock.symlink_metadata().is_ok() {
        std::fs::remove_file(&sock).context("removing the stale socket")?;
    }
    let listener =
        UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    log(format!("listening on {}", sock.display()));

    // crew-mcp runs in the agent's folder and git runs in the project, so
    // both get absolute paths.
    let abs_home =
        std::fs::canonicalize(home).with_context(|| format!("resolving {}", home.display()))?;
    let service = Service::new(
        store,
        opts.max_runs,
        CliBins {
            claude: opts.claude_bin,
            codex: opts.codex_bin,
            runtimes_wait: opts.runtimes_wait,
        },
        abs_home.join(WORKTREES_DIR),
        usable_crew_mcp(opts.crew_mcp_bin).map(|bin| (bin, abs_home)),
        opts.minute,
        opts.notice_buffer,
    );
    service.set_count_delay(opts.count_delay);
    // Folders may have gained or lost repos while crewd was off. Walking
    // them can take a while, so it runs beside serving; clients hear of
    // each change.
    let refresher = service.clone();
    tokio::task::spawn_blocking(move || refresher.refresh_kinds());
    service.clean_snapshots();
    let sweeper = service.sweep_blocked();
    let (stop_terminal, terminal_stopped) = tokio::sync::oneshot::channel();
    let terminal = opts
        .claude_home
        .map(|claude_home| service.watch_terminal(claude_home, terminal_stopped));
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                let service = service.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(service, stream).await {
                        log(format!("connection ended with an error: {e}"));
                    }
                });
            }
            Err(e) => {
                log(format!("accept failed: {e}"));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    log("shutting down, stopping all runs");
    sweeper.abort();
    drop(listener);
    // We still hold the lock, so the socket is ours to remove.
    if let Err(e) = std::fs::remove_file(&sock) {
        log(format!("couldn't remove {}: {e}", sock.display()));
    }
    let _ = stop_terminal.send(());
    if let Some(terminal) = terminal {
        let _ = terminal.await;
    }
    service.shutdown().await;
    log("stopped");
    Ok(())
}

/// Another crewd already serves this home. Not a crash: the `crewd`
/// binary exits 0 on it.
#[derive(Debug)]
pub struct AlreadyRunning(pub PathBuf);

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "crewd is already running on {}", self.0.display())
    }
}

impl std::error::Error for AlreadyRunning {}

/// Takes the exclusive lock on `home/crewd.lock`, or fails if another
/// crewd holds it.
fn lock_home(home: &Path, sock: &Path) -> anyhow::Result<Flock<File>> {
    let path = home.join(LOCK_NAME);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((_, Errno::EWOULDBLOCK)) => Err(AlreadyRunning(sock.to_path_buf()).into()),
        Err((_, e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

async fn respond(
    w: &mut OwnedWriteHalf,
    id: u64,
    result: Result<serde_json::Value, String>,
) -> io::Result<()> {
    write_frame(w, &ServerMsg::Response { id, result }).await
}

/// Said to a request an agent's connection may not make.
pub const NOT_FROM_AGENTS: &str = "Not allowed from inside an agent run.";

/// Who a connection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Who {
    /// The app, the CLI or another client of yours.
    Client,
    /// A process an agent run started: the run, if crewd could tell which.
    Agent(Option<RunId>),
}

/// What a connection from inside an agent run may ask: hand off, and ask
/// you for a permission, both only for its own run. crew-mcp needs
/// nothing else. A connection whose run couldn't be told may only say
/// hello.
fn agent_may(req: &Request, own: Option<RunId>) -> bool {
    match req {
        Request::Hello { .. } => true,
        Request::Handoff { run, .. } | Request::PermissionAsk { run, .. } => own == Some(*run),
        _ => false,
    }
}

/// Whether the connecting process was started by a live run, and which.
async fn peer_of(service: &Service, pid: Option<i32>) -> Who {
    let runs = service.live_run_pids();
    if runs.is_empty() {
        return Who::Client;
    }
    let pids: Vec<i32> = runs.iter().map(|(pid, _)| *pid).collect();
    let peer = tokio::task::spawn_blocking(move || peer::classify(pid, &pids, peer::proc_info))
        .await
        .unwrap_or(Peer::Agent(None));
    match peer {
        Peer::Client => Who::Client,
        Peer::Agent(found) => Who::Agent(
            found.and_then(|f| runs.iter().find(|(pid, _)| *pid == f).map(|(_, run)| *run)),
        ),
    }
}

async fn connection(service: Service, stream: UnixStream) -> io::Result<()> {
    let (uid, pid) = match stream.peer_cred() {
        Ok(cred) => (cred.uid(), cred.pid()),
        // The client already left, as an "is crewd running?" probe does.
        Err(e) if e.kind() == io::ErrorKind::NotConnected => return Ok(()),
        Err(e) => return Err(e),
    };
    if uid != nix::unistd::getuid().as_raw() {
        log(format!("refused a client running as uid {uid}"));
        return Ok(());
    }
    let (mut rd, mut wr) = stream.into_split();

    let Some(ClientMsg::Request { id, req }) = read_frame(&mut rd).await? else {
        return Ok(());
    };
    match req {
        Request::Hello { version } if version == PROTO_VERSION => {
            respond(&mut wr, id, Ok(serde_json::Value::Null)).await?;
        }
        Request::Hello { version } => {
            let msg = format!(
                "crewd speaks protocol {PROTO_VERSION}, client speaks {version}. Restart crewd."
            );
            return respond(&mut wr, id, Err(msg)).await;
        }
        _ => return respond(&mut wr, id, Err("Send Hello first.".into())).await,
    }
    let peer = peer_of(&service, pid).await;

    // Frames are read in their own task: `read_frame` is not safe to cancel
    // inside `select!`, a channel receive is.
    let (req_tx, req_rx) = mpsc::channel(32);
    let reader = tokio::spawn(async move {
        loop {
            match read_frame::<_, ClientMsg>(&mut rd).await {
                Ok(Some(msg)) => {
                    if req_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log(format!("bad frame from a client: {e}"));
                    break;
                }
            }
        }
    });
    let result = requests_and_notices(&service, &mut wr, req_rx, peer).await;
    reader.abort();
    result
}

/// Permission asks made on one connection that still wait for an answer,
/// by request id. Whatever is left when the connection ends is cancelled:
/// nobody would hear the answer.
struct Asks {
    service: Service,
    waiting: HashMap<u64, ApprovalId>,
}

/// The answer to a `SubscribeTerminal`, worked out in its own task: the
/// request id, the session and its backlog.
type TerminalAnswer = (u64, String, Result<Backlog, String>);

/// A `SubscribeTerminal` still being worked out.
#[derive(Default)]
struct Joining {
    /// Items that came meanwhile, sent after the backlog.
    held: Vec<Notice>,
    /// Unsubscribed before the answer came.
    cancelled: bool,
}

/// Terminal sessions one connection subscribes to, each with the serial
/// its backlog covered. Each counts as a subscriber until the connection
/// ends.
struct TerminalSubs {
    service: Service,
    seen: HashMap<String, u64>,
    joining: HashMap<String, Joining>,
    answers: mpsc::Receiver<TerminalAnswer>,
}

impl Drop for TerminalSubs {
    fn drop(&mut self) {
        // Late answers now fail to send, and their tasks let go.
        self.answers.close();
        while let Ok((_, session, result)) = self.answers.try_recv() {
            if result.is_ok() {
                self.service.unsubscribe_terminal(&session);
            }
        }
        for (session, _) in self.seen.drain() {
            self.service.unsubscribe_terminal(&session);
        }
    }
}

impl Drop for Asks {
    fn drop(&mut self) {
        for (_, approval) in self.waiting.drain() {
            self.service.abandon_permission(approval);
        }
    }
}

/// The next notice, or never if the connection has not subscribed.
async fn next_notice(
    notices: &mut Option<broadcast::Receiver<Notice>>,
) -> Result<Notice, broadcast::error::RecvError> {
    match notices {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Whether a connection gets a notice, and so gets each notice at most
/// once. `all` is the serial a `SubscribeAll` snapshot covered, if any;
/// `subscribed` maps each subscribed thread to the serial its backlog
/// covered, and `terminals` each subscribed terminal session to the
/// serial its backlog covered.
fn wanted(
    n: &Notice,
    all: Option<u64>,
    subscribed: &HashMap<ThreadId, u64>,
    terminals: &HashMap<String, u64>,
) -> bool {
    let by_thread = n
        .thread
        .and_then(|t| subscribed.get(&t))
        .is_some_and(|seen| n.serial > *seen);
    let by_all = all.is_some_and(|seen| n.serial > seen);
    match n.msg {
        // Too many for everyone: only for the thread's subscribers.
        ServerMsg::Event { .. } => by_thread,
        ServerMsg::TerminalEvent { ref session, .. } => {
            terminals.get(session).is_some_and(|seen| n.serial > *seen)
        }
        // A session's own subscribers need these even without SubscribeAll.
        ServerMsg::TerminalSessionReset { ref session }
        | ServerMsg::TerminalSessionRemoved { id: ref session } => {
            by_all || terminals.get(session).is_some_and(|seen| n.serial > *seen)
        }
        _ => by_thread || by_all,
    }
}

/// Answers requests and forwards notices for subscribed threads until the
/// client goes away. Runs keep going after that.
///
/// Requests that wait on a person or on git run in their own tasks and are
/// answered when they finish, so later requests are not held up.
async fn requests_and_notices(
    service: &Service,
    wr: &mut OwnedWriteHalf,
    mut req_rx: mpsc::Receiver<ClientMsg>,
    peer: Who,
) -> io::Result<()> {
    // Thread -> serial already covered by that thread's backlog.
    let mut subscribed: HashMap<ThreadId, u64> = HashMap::new();
    // Serial covered by the SubscribeAll snapshot, once it was sent.
    let mut all: Option<u64> = None;
    // Taken on the first Subscribe, so a connection that only asks never
    // falls behind.
    let mut notices: Option<broadcast::Receiver<Notice>> = None;
    // Answers to requests handled in their own tasks.
    let (answer_tx, mut answer_rx) = mpsc::channel::<(u64, Reply)>(8);
    let mut asks = Asks {
        service: service.clone(),
        waiting: HashMap::new(),
    };
    let (terminal_tx, terminal_rx) = mpsc::channel::<TerminalAnswer>(8);
    let mut terminals = TerminalSubs {
        service: service.clone(),
        seen: HashMap::new(),
        joining: HashMap::new(),
        answers: terminal_rx,
    };
    loop {
        tokio::select! {
            Some((id, session, result)) = terminals.answers.recv() => {
                let joining = terminals.joining.remove(&session).unwrap_or_default();
                match result {
                    Ok(_) if joining.cancelled => {
                        service.unsubscribe_terminal(&session);
                        respond(wr, id, Ok(serde_json::Value::Null)).await?;
                    }
                    Ok(backlog) => {
                        // One count per connection, kept until it ends.
                        if terminals.seen.insert(session.clone(), backlog.serial).is_some() {
                            service.unsubscribe_terminal(&session);
                        }
                        respond(wr, id, Ok(serde_json::Value::Null)).await?;
                        for msg in &backlog.msgs {
                            write_frame(wr, msg).await?;
                        }
                        for n in joining.held {
                            if n.serial > backlog.serial {
                                write_frame(wr, &n.msg).await?;
                            }
                        }
                    }
                    Err(e) => respond(wr, id, Err(e)).await?,
                }
            }
            Some((id, result)) = answer_rx.recv() => {
                asks.waiting.remove(&id);
                respond(wr, id, result).await?;
            }
            msg = req_rx.recv() => {
                let Some(ClientMsg::Request { id, req }) = msg else {
                    return Ok(());
                };
                if let Who::Agent(own) = peer
                    && !agent_may(&req, own)
                {
                    respond(wr, id, Err(NOT_FROM_AGENTS.into())).await?;
                    continue;
                }
                match req {
                    Request::Subscribe { thread } => {
                        // Taken before the backlog is read, so nothing falls
                        // between the two.
                        notices.get_or_insert_with(|| service.notices());
                        match service.subscribe(thread) {
                            Ok(backlog) => {
                                respond(wr, id, Ok(serde_json::Value::Null)).await?;
                                for msg in &backlog.msgs {
                                    write_frame(wr, msg).await?;
                                }
                                subscribed.insert(thread, backlog.serial);
                            }
                            Err(e) => respond(wr, id, Err(e.to_string())).await?,
                        }
                    }
                    Request::SubscribeAll => {
                        notices.get_or_insert_with(|| service.notices());
                        match service.snapshot() {
                            Ok((snapshot, serial)) => {
                                respond(wr, id, Ok(serde_json::Value::Null)).await?;
                                write_frame(wr, &snapshot).await?;
                                all = Some(serial);
                            }
                            Err(e) => respond(wr, id, Err(e.to_string())).await?,
                        }
                    }
                    Request::SubscribeTerminal { session } => {
                        if terminals.joining.contains_key(&session) {
                            let msg = "Already subscribing to that terminal session.";
                            respond(wr, id, Err(msg.into())).await?;
                            continue;
                        }
                        // Taken before the backlog is read, so nothing falls
                        // between the two.
                        notices.get_or_insert_with(|| service.notices());
                        terminals.joining.insert(session.clone(), Joining::default());
                        // Reading the backlog can take a while: answered
                        // when it is ready.
                        let (svc, tx) = (service.clone(), terminal_tx.clone());
                        tokio::spawn(async move {
                            let result = match svc.subscribe_terminal(&session).await {
                                Ok(()) => Ok(svc.terminal_backlog(&session)),
                                Err(e) => Err(e),
                            };
                            if let Err(mpsc::error::SendError((_, session, Ok(_)))) =
                                tx.send((id, session, result)).await
                            {
                                // The connection is gone.
                                svc.unsubscribe_terminal(&session);
                            }
                        });
                    }
                    Request::UnsubscribeTerminal { session } => {
                        if let Some(joining) = terminals.joining.get_mut(&session) {
                            joining.cancelled = true;
                        }
                        if terminals.seen.remove(&session).is_some() {
                            service.unsubscribe_terminal(&session);
                        }
                        respond(wr, id, Ok(serde_json::Value::Null)).await?;
                    }
                    Request::Hello { .. } => {
                        respond(wr, id, Err("Hello was already sent.".into())).await?;
                    }
                    Request::PermissionAsk { run, tool, input, tool_use_id } => {
                        match service.ask_permission(run, &tool, input, tool_use_id) {
                            Ok(wait) => {
                                if let Some(approval) = wait.approval {
                                    asks.waiting.insert(id, approval);
                                }
                                let tx = answer_tx.clone();
                                tokio::spawn(async move {
                                    // A dropped sender means the run is gone.
                                    let answer =
                                        wait.answer.await.unwrap_or_else(|_| stopped_answer());
                                    let result =
                                        serde_json::to_value(answer).map_err(|e| e.to_string());
                                    let _ = tx.send((id, result)).await;
                                });
                            }
                            Err(e) => respond(wr, id, Err(e)).await?,
                        }
                    }
                    req @ (Request::ThreadDiff { .. }
                    | Request::Commit { .. }
                    | Request::Discard { .. }
                    | Request::ChannelOverview { .. }
                    | Request::Runtimes) => {
                        let (svc, tx) = (service.clone(), answer_tx.clone());
                        tokio::spawn(async move {
                            let result = svc.handle(req).await;
                            let _ = tx.send((id, result)).await;
                        });
                    }
                    other => {
                        let result = service.handle(other).await;
                        respond(wr, id, result).await?;
                    }
                }
            }
            notice = next_notice(&mut notices) => match notice {
                Ok(n) => {
                    if let ServerMsg::TerminalEvent { session, .. } = &n.msg
                        && let Some(joining) = terminals.joining.get_mut(session)
                        && !joining.cancelled
                    {
                        joining.held.push(n);
                    } else if wanted(&n, all, &subscribed, &terminals.seen) {
                        write_frame(wr, &n.msg).await?;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    log(format!(
                        "a client fell behind and missed {missed} notifications, asking it to resync"
                    ));
                    return write_frame(wr, &ServerMsg::Resync).await;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(serial: u64, msg: ServerMsg) -> Notice {
        Notice {
            serial,
            thread: None,
            msg,
        }
    }

    #[test]
    fn agents_ask_only_as_their_own_run() {
        let (a, b) = (RunId::new(), RunId::new());
        let handoff = |run| Request::Handoff {
            run,
            agent: "Forge".into(),
            brief: "x".into(),
        };
        let ask = |run| Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input: serde_json::Value::Null,
            tool_use_id: None,
        };
        assert!(agent_may(&handoff(a), Some(a)));
        assert!(agent_may(&ask(a), Some(a)));
        assert!(!agent_may(&handoff(b), Some(a)));
        assert!(!agent_may(&ask(b), Some(a)));
        // A run that couldn't be told asks for nothing.
        assert!(!agent_may(&handoff(a), None));
        assert!(!agent_may(&ask(a), None));
        let hello = Request::Hello {
            version: PROTO_VERSION,
        };
        assert!(agent_may(&hello, None));
        assert!(!agent_may(&Request::StopAll, Some(a)));
    }

    #[test]
    fn claude_home_prefers_crew_then_claude_settings() {
        let v = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(
            claude_home_from(v("/work/a"), v("/work/b"), v("/work/h")),
            Some(PathBuf::from("/work/a"))
        );
        assert_eq!(
            claude_home_from(v(""), v("/work/b"), v("/work/h")),
            Some(PathBuf::from("/work/b"))
        );
        assert_eq!(
            claude_home_from(None, None, v("/work/h")),
            Some(PathBuf::from("/work/h/.claude"))
        );
        assert_eq!(claude_home_from(None, v(""), None), None);
    }

    #[test]
    fn terminal_subscribers_hear_resets_and_removals() {
        let none: HashMap<ThreadId, u64> = HashMap::new();
        let no_terminals: HashMap<String, u64> = HashMap::new();
        let mut terminals = HashMap::new();
        terminals.insert("s1".to_string(), 5u64);
        for msg in [
            ServerMsg::TerminalSessionReset {
                session: "s1".into(),
            },
            ServerMsg::TerminalSessionRemoved { id: "s1".into() },
        ] {
            assert!(wanted(&notice(6, msg.clone()), None, &none, &terminals));
            // Covered by its backlog already.
            assert!(!wanted(&notice(5, msg.clone()), None, &none, &terminals));
            // Not subscribed to that session.
            assert!(!wanted(&notice(6, msg.clone()), None, &none, &no_terminals));
            assert!(wanted(&notice(6, msg), Some(0), &none, &no_terminals));
        }
    }
}
