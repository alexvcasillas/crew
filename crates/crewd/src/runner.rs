//! Runs one `claude` process and streams its events.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use crew_core::claude::{ClaudeInvocation, parse_line};
use crew_core::event::Event;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// Shown when the `claude` binary can't be found.
pub const MISSING_CLAUDE: &str = "Couldn't start `claude`. Is Claude Code installed?";

/// Time between SIGINT and SIGTERM when stopping.
const TERM_AFTER: Duration = Duration::from_secs(3);
/// Time between SIGTERM and SIGKILL when stopping.
const KILL_AFTER: Duration = Duration::from_secs(5);
/// How many stderr lines a failure keeps.
const STDERR_TAIL: usize = 20;
/// Capacity of the event channel.
const EVENT_BUFFER: usize = 1024;

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    /// The turn finished well and the process exited 0.
    Done,
    /// The run failed; holds a message for the user.
    Failed(String),
    /// The run was stopped on request.
    Stopped,
}

/// Controls a running process.
///
/// Dropping the handle stops the run, the same as calling [`RunHandle::stop`].
/// Keep it alive for as long as the run should go on.
pub struct RunHandle {
    pid: Option<u32>,
    stop_tx: watch::Sender<bool>,
}

impl RunHandle {
    /// The process id, or `None` if the process never started.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Asks the run to stop. Safe to call more than once or after exit.
    pub fn stop(&self) {
        self.stop_tx.send_replace(true);
    }
}

/// Spawns `claude` in its own process group and streams its events.
///
/// Must be called inside a tokio runtime. Dropping the returned
/// [`RunHandle`] stops the run.
pub fn spawn_run(
    inv: ClaudeInvocation,
) -> (RunHandle, mpsc::Receiver<Event>, JoinHandle<RunOutcome>) {
    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    let (stop_tx, stop_rx) = watch::channel(false);

    if !inv.cwd.is_dir() {
        let msg = format!("Working folder {} doesn't exist.", inv.cwd.display());
        let handle = RunHandle { pid: None, stop_tx };
        return (handle, rx, tokio::spawn(async { RunOutcome::Failed(msg) }));
    }

    let spawned = Command::new(&inv.bin)
        .args(&inv.args)
        .current_dir(&inv.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();

    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let msg = if e.kind() == ErrorKind::NotFound {
                MISSING_CLAUDE.to_string()
            } else {
                format!("Couldn't start `claude`: {e}")
            };
            let handle = RunHandle { pid: None, stop_tx };
            return (handle, rx, tokio::spawn(async { RunOutcome::Failed(msg) }));
        }
    };

    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout_task = tokio::spawn(read_events(stdout, tx));
    let stderr_task = tokio::spawn(read_tail(stderr));
    let join = tokio::spawn(supervise(child, pid, stop_rx, stdout_task, stderr_task));

    (RunHandle { pid, stop_tx }, rx, join)
}

/// Parses stdout into events. Returns true if a good `TurnEnd` was seen.
///
/// Keeps reading to the end even if the receiver is gone, so the child
/// never blocks on a full pipe.
async fn read_events(stdout: impl AsyncRead + Unpin, tx: mpsc::Sender<Event>) -> bool {
    let mut reader = BufReader::new(stdout);
    let mut turn_ok = false;
    let mut receiver_alive = true;
    while let Some(line) = next_line(&mut reader).await {
        for event in parse_line(&line) {
            if let Event::TurnEnd { ok, .. } = &event {
                turn_ok = *ok;
            }
            if receiver_alive && tx.send(event).await.is_err() {
                receiver_alive = false;
            }
        }
    }
    turn_ok
}

/// Keeps the last stderr lines.
async fn read_tail(stderr: impl AsyncRead + Unpin) -> VecDeque<String> {
    let mut reader = BufReader::new(stderr);
    let mut tail = VecDeque::with_capacity(STDERR_TAIL);
    while let Some(line) = next_line(&mut reader).await {
        if tail.len() == STDERR_TAIL {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    tail
}

/// Reads one line, replacing bad UTF-8. Returns `None` at end of input or
/// on a read error.
async fn next_line(reader: &mut (impl AsyncBufRead + Unpin)) -> Option<String> {
    let mut buf = Vec::new();
    match reader.read_until(b'\n', &mut buf).await {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            let line = String::from_utf8_lossy(&buf);
            Some(line.trim_end_matches(['\n', '\r']).to_string())
        }
    }
}

async fn supervise(
    mut child: Child,
    pid: Option<u32>,
    mut stop_rx: watch::Receiver<bool>,
    stdout_task: JoinHandle<bool>,
    stderr_task: JoinHandle<VecDeque<String>>,
) -> RunOutcome {
    let pgid = pid.map(|p| Pid::from_raw(p as i32));

    let exited = tokio::select! {
        status = child.wait() => Some(status),
        stop = stop_rx.wait_for(|stop| *stop) => match stop {
            Ok(_) => None,
            // The handle was dropped: stop the run so it can't outlive its owner.
            Err(_) => None,
        },
    };

    let status = match exited {
        Some(status) => status,
        None => {
            escalate(&mut child, pgid).await;
            signal_group(pgid, Signal::SIGKILL);
            return RunOutcome::Stopped;
        }
    };

    // Clear anything the process left behind in its group.
    signal_group(pgid, Signal::SIGKILL);

    let turn_ok = stdout_task.await.unwrap_or(false);
    let tail = stderr_task.await.unwrap_or_default();
    match status {
        Ok(s) if s.success() && turn_ok => RunOutcome::Done,
        Ok(s) if s.success() => {
            RunOutcome::Failed("`claude` exited before finishing the turn.".to_string())
        }
        Ok(s) => RunOutcome::Failed(failure_message(s, tail)),
        Err(e) => RunOutcome::Failed(format!("Lost track of `claude`: {e}")),
    }
}

/// Sends SIGINT, then SIGTERM, then SIGKILL until the process exits.
async fn escalate(child: &mut Child, pgid: Option<Pid>) {
    signal_group(pgid, Signal::SIGINT);
    if tokio::time::timeout(TERM_AFTER, child.wait()).await.is_ok() {
        return;
    }
    signal_group(pgid, Signal::SIGTERM);
    if tokio::time::timeout(KILL_AFTER, child.wait()).await.is_ok() {
        return;
    }
    signal_group(pgid, Signal::SIGKILL);
    let _ = child.wait().await;
}

fn signal_group(pgid: Option<Pid>, signal: Signal) {
    if let Some(pgid) = pgid {
        // The group may already be gone; that is fine.
        let _ = killpg(pgid, signal);
    }
}

fn failure_message(status: ExitStatus, tail: VecDeque<String>) -> String {
    if !tail.is_empty() {
        return Vec::from(tail).join("\n");
    }
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("`claude` exited with code {code}."),
        (None, Some(sig)) => format!("`claude` was ended by signal {sig}."),
        (None, None) => format!("`claude` exited: {status}."),
    }
}
