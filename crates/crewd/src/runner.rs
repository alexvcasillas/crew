//! Runs one `claude` process and streams its events.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
/// How long the output readers may run on after the process is gone. A
/// leftover child that holds the pipes would otherwise keep them open.
const READERS_GRACE: Duration = Duration::from_secs(2);

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
    let turn_ok = Arc::new(AtomicBool::new(false));
    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
    let readers = Readers {
        stdout: tokio::spawn(read_events(stdout, tx, turn_ok.clone())),
        stderr: tokio::spawn(read_tail(stderr, tail.clone())),
        turn_ok,
        tail,
    };
    let join = tokio::spawn(supervise(child, pid, stop_rx, readers));

    (RunHandle { pid, stop_tx }, rx, join)
}

/// The output readers and what they found so far. The findings live
/// outside the tasks, so they survive when a reader is aborted.
struct Readers {
    /// Whether the last `TurnEnd` seen was good.
    turn_ok: Arc<AtomicBool>,
    /// The last stderr lines.
    tail: Arc<Mutex<VecDeque<String>>>,
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

/// Parses stdout into events and records each `TurnEnd` in `turn_ok`.
///
/// Keeps reading to the end even if the receiver is gone, so the child
/// never blocks on a full pipe.
async fn read_events(
    stdout: impl AsyncRead + Unpin,
    tx: mpsc::Sender<Event>,
    turn_ok: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    let mut receiver_alive = true;
    while let Some(line) = next_line(&mut reader).await {
        for event in parse_line(&line) {
            if let Event::TurnEnd { ok, .. } = &event {
                turn_ok.store(*ok, Ordering::SeqCst);
            }
            if receiver_alive && tx.send(event).await.is_err() {
                receiver_alive = false;
            }
        }
    }
}

/// Keeps the last stderr lines in `tail`, updated per line.
async fn read_tail(stderr: impl AsyncRead + Unpin, tail: Arc<Mutex<VecDeque<String>>>) {
    let mut reader = BufReader::new(stderr);
    while let Some(line) = next_line(&mut reader).await {
        let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
        if tail.len() == STDERR_TAIL {
            tail.pop_front();
        }
        tail.push_back(line);
    }
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
    readers: Readers,
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
            // Ends the readers, which closes the event channel.
            finish_readers(readers).await;
            return RunOutcome::Stopped;
        }
    };

    // Clear anything the process left behind in its group.
    signal_group(pgid, Signal::SIGKILL);

    let (turn_ok, tail) = finish_readers(readers).await;
    match status {
        Ok(s) if s.success() && turn_ok => RunOutcome::Done,
        Ok(s) if s.success() => {
            RunOutcome::Failed("`claude` exited before finishing the turn.".to_string())
        }
        Ok(s) => RunOutcome::Failed(failure_message(s, tail)),
        Err(e) => RunOutcome::Failed(format!("Lost track of `claude`: {e}")),
    }
}

/// Waits up to [`READERS_GRACE`] for the readers, then aborts them. Either
/// way the stdout reader's sender is dropped, so the event channel closes.
/// Returns what the readers found, including lines read before an abort.
async fn finish_readers(readers: Readers) -> (bool, VecDeque<String>) {
    let deadline = tokio::time::Instant::now() + READERS_GRACE;
    join_until(readers.stdout, deadline).await;
    join_until(readers.stderr, deadline).await;
    let turn_ok = readers.turn_ok.load(Ordering::SeqCst);
    let tail = std::mem::take(&mut *readers.tail.lock().unwrap_or_else(PoisonError::into_inner));
    (turn_ok, tail)
}

/// Waits for `task` until `deadline`, then aborts it.
async fn join_until(mut task: JoinHandle<()>, deadline: tokio::time::Instant) {
    if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
        task.abort();
        // Wait for the abort so the task is gone, and with it the sender.
        let _ = task.await;
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
