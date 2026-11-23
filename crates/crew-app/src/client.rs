//! The connection to crewd: requests, and what crewd tells the app.
//!
//! All socket IO runs on one thread of its own with a small tokio runtime,
//! never on the UI thread. What crewd sends reaches the UI as
//! [`ClientEvent`]s through an unbounded channel.
//!
//! The client keeps two connections to crewd:
//! - the stream, which subscribes to everything (`SubscribeAll`) and to
//!   each watched thread (`Subscribe`), and
//! - the control connection, which carries requests, answered by id.
//!
//! Requests get their own connection because crewd closes a stream that
//! falls behind (`Resync`). On a shared connection that would fail
//! requests still waiting for an answer, such as a commit.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::ids::ThreadId;
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::connection::{self, ConnectionConfig, PROBE_TIMEOUT};
use crate::model::{ConnectionState, State};

/// First pause before connecting again. It doubles up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// A request made while crewd isn't connected.
const NOT_CONNECTED: &str = "Crew's service isn't running. Start it, then try again.";
/// A request whose connection closed before it was answered.
const LOST: &str = "Crew lost its connection to its service. Try again.";

/// What the client tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// Both connections are up. A `Snapshot` follows.
    Connected,
    /// crewd isn't there, or the connection dropped. The client keeps
    /// trying.
    Disconnected(String),
    /// crewd refused this app's protocol version. The client keeps trying,
    /// for when crewd is restarted.
    VersionMismatch { theirs: u32 },
    /// A notice from the stream.
    Msg(ServerMsg),
}

impl ClientEvent {
    /// Applies the event to the model.
    pub fn apply_to(self, state: &mut State) {
        match self {
            ClientEvent::Connected => state.set_connection(ConnectionState::Connected),
            ClientEvent::Disconnected(_) => state.set_connection(ConnectionState::NotRunning),
            ClientEvent::VersionMismatch { theirs } => {
                state.set_connection(ConnectionState::VersionMismatch {
                    theirs,
                    ours: PROTO_VERSION,
                });
            }
            ClientEvent::Msg(msg) => state.apply(msg),
        }
    }
}

enum Command {
    Request(Request, oneshot::Sender<Result<Value, String>>),
    Watch(ThreadId),
    Unwatch(ThreadId),
    Reconnect,
}

/// Talks to the client thread. Cheap to clone; the thread stops once
/// every handle is dropped.
#[derive(Clone)]
pub struct ClientHandle {
    commands: mpsc::UnboundedSender<Command>,
}

impl ClientHandle {
    /// Sends `req` on the control connection. Resolves to crewd's answer,
    /// or an error to show if crewd isn't connected or the connection drops
    /// first.
    pub fn request(
        &self,
        req: Request,
    ) -> impl Future<Output = Result<Value, String>> + Send + use<> {
        let (reply, answer) = oneshot::channel();
        let sent = self.commands.send(Command::Request(req, reply)).is_ok();
        async move {
            if !sent {
                return Err(LOST.into());
            }
            answer.await.unwrap_or_else(|_| Err(LOST.into()))
        }
    }

    /// Subscribes to `thread`, now and after every reconnect. crewd sends
    /// its backlog, then its live events.
    pub fn watch_thread(&self, thread: ThreadId) {
        let _ = self.commands.send(Command::Watch(thread));
    }

    /// Stops resubscribing to `thread`. crewd can't unsubscribe, so its
    /// events keep coming on this connection; the model drops them.
    pub fn unwatch_thread(&self, thread: ThreadId) {
        let _ = self.commands.send(Command::Unwatch(thread));
    }

    /// Connects now instead of waiting out the backoff, as after starting
    /// crewd.
    pub fn reconnect_now(&self) {
        let _ = self.commands.send(Command::Reconnect);
    }
}

/// Starts the client thread for crewd on `conn`.
pub fn spawn(conn: ConnectionConfig) -> (ClientHandle, UnboundedReceiver<ClientEvent>) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (events, event_rx) = unbounded();
    let socket = conn.socket();
    std::thread::Builder::new()
        .name("crew-client".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build();
            match runtime {
                Ok(rt) => rt.block_on(run(socket, command_rx, events)),
                Err(e) => {
                    let _ = events.unbounded_send(ClientEvent::Disconnected(e.to_string()));
                }
            }
        })
        .expect("starting the client thread");
    (ClientHandle { commands }, event_rx)
}

/// Connects, serves the connection until it drops, waits and connects
/// again, until every handle or the event receiver is gone.
async fn run(
    socket: PathBuf,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: UnboundedSender<ClientEvent>,
) {
    let mut watched: Vec<ThreadId> = Vec::new();
    let mut backoff = MIN_BACKOFF;
    let mut last: Option<ClientEvent> = None;
    loop {
        let outcome = match open_both(&socket).await {
            Ok((stream, control)) => {
                backoff = MIN_BACKOFF;
                if events.unbounded_send(ClientEvent::Connected).is_err() {
                    return;
                }
                last = Some(ClientEvent::Connected);
                match session(stream, control, &mut commands, &events, &mut watched).await {
                    Ended::Shutdown => return,
                    Ended::Lost(reason) => ClientEvent::Disconnected(reason),
                }
            }
            Err(Refused::Version(theirs)) => ClientEvent::VersionMismatch { theirs },
            Err(Refused::Other(reason)) => ClientEvent::Disconnected(reason),
        };
        // Failing again the same way is reported once.
        let repeat = last
            .as_ref()
            .is_some_and(|l| std::mem::discriminant(l) == std::mem::discriminant(&outcome));
        if !repeat {
            if events.unbounded_send(outcome.clone()).is_err() {
                return;
            }
            last = Some(outcome);
        }
        match pause(backoff, &mut commands, &mut watched).await {
            Pause::Shutdown => return,
            Pause::Elapsed => backoff = (backoff * 2).min(MAX_BACKOFF),
            Pause::Reconnect => backoff = MIN_BACKOFF,
        }
        if events.is_closed() {
            return;
        }
    }
}

enum Refused {
    Version(u32),
    Other(String),
}

/// Opens the stream and the control connection, each past `Hello`.
async fn open_both(socket: &Path) -> Result<(UnixStream, UnixStream), Refused> {
    let stream = open(socket).await?;
    let control = open(socket).await?;
    Ok((stream, control))
}

async fn open(socket: &Path) -> Result<UnixStream, Refused> {
    match tokio::time::timeout(PROBE_TIMEOUT, connection::hello(socket)).await {
        Ok(Ok(Some((stream, Ok(()))))) => Ok(stream),
        Ok(Ok(Some((_, Err(refusal))))) => match connection::their_version(&refusal) {
            Some(theirs) => Err(Refused::Version(theirs)),
            None => Err(Refused::Other(refusal)),
        },
        Ok(Ok(None)) => Err(Refused::Other("crewd closed the connection.".into())),
        Ok(Err(e)) => Err(Refused::Other(e.to_string())),
        Err(_) => Err(Refused::Other("crewd didn't answer.".into())),
    }
}

enum Ended {
    /// Every handle or the event receiver is gone.
    Shutdown,
    Lost(String),
}

#[derive(Clone, Copy)]
enum Side {
    Stream,
    Control,
}

type Frame = (Side, io::Result<Option<ServerMsg>>);

/// Aborts the reader tasks when the session ends, closing their halves.
struct Readers([JoinHandle<()>; 2]);

impl Drop for Readers {
    fn drop(&mut self) {
        for reader in &self.0 {
            reader.abort();
        }
    }
}

/// Subscribes, then forwards notices and answers requests until a
/// connection drops.
async fn session(
    stream: UnixStream,
    control: UnixStream,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &UnboundedSender<ClientEvent>,
    watched: &mut Vec<ThreadId>,
) -> Ended {
    let (stream_rd, mut stream_wr) = stream.into_split();
    let (control_rd, mut control_wr) = control.into_split();
    // `read_frame` can't be cancelled inside `select!`, a channel can.
    let (frames_tx, mut frames) = mpsc::unbounded_channel::<Frame>();
    let _readers = Readers([
        tokio::spawn(read_into(stream_rd, Side::Stream, frames_tx.clone())),
        tokio::spawn(read_into(control_rd, Side::Control, frames_tx)),
    ]);
    let mut next_id = 1;
    let mut waiting: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();

    let subscribe = std::iter::once(Request::SubscribeAll)
        .chain(watched.iter().map(|&thread| Request::Subscribe { thread }));
    for req in subscribe {
        if let Err(e) = send(&mut stream_wr, &mut next_id, req).await {
            return Ended::Lost(e.to_string());
        }
    }

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => return Ended::Shutdown,
                Some(Command::Request(req, reply)) => {
                    let id = next_id;
                    if let Err(e) = send(&mut control_wr, &mut next_id, req).await {
                        let _ = reply.send(Err(LOST.into()));
                        return Ended::Lost(e.to_string());
                    }
                    waiting.insert(id, reply);
                }
                Some(Command::Watch(thread)) => {
                    if !watched.contains(&thread) {
                        watched.push(thread);
                        let req = Request::Subscribe { thread };
                        if let Err(e) = send(&mut stream_wr, &mut next_id, req).await {
                            return Ended::Lost(e.to_string());
                        }
                    }
                }
                Some(Command::Unwatch(thread)) => watched.retain(|t| *t != thread),
                Some(Command::Reconnect) => {}
            },
            Some((side, frame)) = frames.recv() => {
                let msg = match frame {
                    Ok(Some(msg)) => msg,
                    Ok(None) => return Ended::Lost("crewd closed the connection.".into()),
                    Err(e) => return Ended::Lost(e.to_string()),
                };
                match (side, msg) {
                    (Side::Control, ServerMsg::Response { id, result }) => {
                        if let Some(reply) = waiting.remove(&id) {
                            let _ = reply.send(result);
                        }
                    }
                    (Side::Control, _) => {}
                    (Side::Stream, ServerMsg::Response { result: Err(e), .. }) => {
                        eprintln!("crew: crewd refused a subscription: {e}");
                    }
                    (Side::Stream, ServerMsg::Response { .. }) => {}
                    (Side::Stream, ServerMsg::Resync) => {
                        return Ended::Lost("crewd asked to catch up.".into());
                    }
                    (Side::Stream, msg) => {
                        if events.unbounded_send(ClientEvent::Msg(msg)).is_err() {
                            return Ended::Shutdown;
                        }
                    }
                }
            }
        }
    }
}

async fn send(wr: &mut OwnedWriteHalf, next_id: &mut u64, req: Request) -> io::Result<()> {
    let id = *next_id;
    *next_id += 1;
    write_frame(wr, &ClientMsg::Request { id, req }).await
}

/// Reads frames into `tx` until the connection ends, sending that end too.
async fn read_into(mut rd: OwnedReadHalf, side: Side, tx: mpsc::UnboundedSender<Frame>) {
    loop {
        let frame = read_frame(&mut rd).await;
        let more = matches!(frame, Ok(Some(_)));
        if tx.send((side, frame)).is_err() || !more {
            return;
        }
    }
}

enum Pause {
    Elapsed,
    Reconnect,
    Shutdown,
}

/// Waits `time` before connecting again. Requests made meanwhile fail
/// right away; watches are remembered for the next connection.
async fn pause(
    time: Duration,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    watched: &mut Vec<ThreadId>,
) -> Pause {
    let sleep = tokio::time::sleep(time);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return Pause::Elapsed,
            command = commands.recv() => match command {
                None => return Pause::Shutdown,
                Some(Command::Request(_, reply)) => {
                    let _ = reply.send(Err(NOT_CONNECTED.into()));
                }
                Some(Command::Watch(thread)) => {
                    if !watched.contains(&thread) {
                        watched.push(thread);
                    }
                }
                Some(Command::Unwatch(thread)) => watched.retain(|t| *t != thread),
                Some(Command::Reconnect) => return Pause::Reconnect,
            },
        }
    }
}
