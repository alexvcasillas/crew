//! The connection to crewd: requests, and what crewd tells the app.
//!
//! All socket IO runs on one thread of its own with a small tokio runtime,
//! never on the UI thread. What crewd sends reaches the UI as
//! [`ClientEvent`]s through an unbounded channel.
//!
//! The client keeps two connections to crewd:
//! - the stream, which subscribes to everything (`SubscribeAll`), to
//!   each watched thread (`Subscribe`) and to each watched terminal
//!   session (`SubscribeTerminal`), and
//! - the control connection, which carries requests, answered by id.
//!
//! Requests get their own connection because crewd closes a stream that
//! falls behind (`Resync`). Only the stream is reopened then, so requests
//! still waiting for an answer, such as a commit, keep waiting.

use std::collections::{HashMap, HashSet};
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

/// A request made while nothing listens on crewd's socket.
pub const NOT_RUNNING: &str = "Crew's service isn't running. Start it, then try again.";
/// A request made while crewd refuses this app's protocol version.
pub const OUT_OF_DATE: &str = "Crew's service is out of date. Restart it, then try again.";
/// A request made between losing crewd and connecting again.
pub const RECONNECTING: &str = "Crew is reconnecting to its service. Try again in a moment.";
/// A request whose connection closed before it was answered.
pub const LOST: &str = "Crew lost its connection to its service. Try again.";

/// What the client tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// Both connections are up. A `Snapshot` follows.
    Connected,
    /// A connection dropped or crewd misbehaved. The client is connecting
    /// again.
    Reconnecting(String),
    /// Nothing listens on crewd's socket: it is missing or refuses
    /// connections. The client keeps trying.
    NotRunning(io::ErrorKind),
    /// crewd refused this app's protocol version. The client keeps trying,
    /// for when crewd is restarted.
    VersionMismatch { theirs: u32 },
    /// A notice from the stream. After the stream is reopened, a fresh
    /// `Snapshot` and the watched threads' backlogs come again.
    Msg(ServerMsg),
}

impl ClientEvent {
    /// Applies the event to the model.
    pub fn apply_to(self, state: &mut State) {
        match self {
            ClientEvent::Connected => state.set_connection(ConnectionState::Connected),
            ClientEvent::Reconnecting(_) => state.set_connection(ConnectionState::Reconnecting),
            ClientEvent::NotRunning(_) => state.set_connection(ConnectionState::NotRunning),
            ClientEvent::VersionMismatch { theirs } => {
                state.set_connection(ConnectionState::VersionMismatch {
                    theirs,
                    ours: PROTO_VERSION,
                });
            }
            ClientEvent::Msg(msg) => state.apply(msg),
        }
    }

    /// What a request made while this is the state gets.
    fn refusal(&self) -> &'static str {
        match self {
            ClientEvent::NotRunning(_) => NOT_RUNNING,
            ClientEvent::VersionMismatch { .. } => OUT_OF_DATE,
            _ => RECONNECTING,
        }
    }
}

enum Command {
    Request(Request, oneshot::Sender<Result<Value, String>>),
    Watch(ThreadId),
    Unwatch(ThreadId),
    WatchTerminal(String),
    UnwatchTerminal(String),
    Reconnect,
}

/// What the stream subscribes to, besides everything.
#[derive(Default)]
struct Watched {
    threads: Vec<ThreadId>,
    terminals: Vec<String>,
}

impl Watched {
    /// Notes a watch command. Returns the request to send on an open
    /// stream, if any. Other commands change nothing.
    fn take(&mut self, command: Command) -> Option<Request> {
        match command {
            Command::Watch(thread) => (!self.threads.contains(&thread)).then(|| {
                self.threads.push(thread);
                Request::Subscribe { thread }
            }),
            Command::Unwatch(thread) => {
                self.threads.retain(|t| *t != thread);
                None
            }
            Command::WatchTerminal(session) => (!self.terminals.contains(&session)).then(|| {
                self.terminals.push(session.clone());
                Request::SubscribeTerminal { session }
            }),
            Command::UnwatchTerminal(session) => {
                let watched = self.terminals.contains(&session);
                self.terminals.retain(|s| *s != session);
                watched.then_some(Request::UnsubscribeTerminal { session })
            }
            Command::Request(..) | Command::Reconnect => None,
        }
    }

    /// Everything a new stream subscribes to, in order.
    fn requests(&self) -> impl Iterator<Item = Request> + '_ {
        let threads = self
            .threads
            .iter()
            .map(|&thread| Request::Subscribe { thread });
        let terminals = self
            .terminals
            .iter()
            .map(|session| Request::SubscribeTerminal {
                session: session.clone(),
            });
        std::iter::once(Request::SubscribeAll)
            .chain(threads)
            .chain(terminals)
    }
}

/// Talks to the client thread. Cheap to clone; the thread stops once
/// every handle, or the event receiver, is dropped.
#[derive(Clone)]
pub struct ClientHandle {
    commands: mpsc::UnboundedSender<Command>,
}

impl ClientHandle {
    /// Sends `req` on the control connection. Resolves to crewd's answer,
    /// or an error to show if crewd isn't connected or the control
    /// connection drops first.
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

    /// Subscribes to a terminal session, now, after every reconnect and
    /// after crewd resets it. crewd sends its last items, then new ones.
    pub fn watch_terminal(&self, session: String) {
        let _ = self.commands.send(Command::WatchTerminal(session));
    }

    /// Unsubscribes from a terminal session.
    pub fn unwatch_terminal(&self, session: String) {
        let _ = self.commands.send(Command::UnwatchTerminal(session));
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
                    let _ = events.unbounded_send(ClientEvent::Reconnecting(e.to_string()));
                }
            }
        })
        .expect("starting the client thread");
    (ClientHandle { commands }, event_rx)
}

/// The connection status last told to the UI. The same kind of failure
/// is told once.
struct Status<'a> {
    events: &'a UnboundedSender<ClientEvent>,
    last: Option<ClientEvent>,
}

impl Status<'_> {
    /// Tells the UI `status` unless it is a repeat. False once nobody
    /// listens.
    fn report(&mut self, status: ClientEvent) -> bool {
        let repeat = self
            .last
            .as_ref()
            .is_some_and(|l| std::mem::discriminant(l) == std::mem::discriminant(&status));
        if repeat {
            return true;
        }
        self.last = Some(status.clone());
        self.events.unbounded_send(status).is_ok()
    }

    fn send(&self, event: ClientEvent) -> bool {
        self.events.unbounded_send(event).is_ok()
    }
}

/// Connects, serves the connections until the control one drops, waits
/// and connects again, until every handle or the event receiver is gone.
async fn run(
    socket: PathBuf,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: UnboundedSender<ClientEvent>,
) {
    let mut watched = Watched::default();
    let mut backoff = MIN_BACKOFF;
    let mut status = Status {
        events: &events,
        last: None,
    };
    loop {
        let outcome = match session(&socket, &mut commands, &mut status, &mut watched).await {
            Ended::Shutdown => return,
            Ended::Lost(reason) => {
                backoff = MIN_BACKOFF;
                ClientEvent::Reconnecting(reason)
            }
            Ended::Refused(Refused::Version(theirs)) => ClientEvent::VersionMismatch { theirs },
            Ended::Refused(Refused::NotRunning(kind)) => ClientEvent::NotRunning(kind),
            Ended::Refused(Refused::Other(reason)) => ClientEvent::Reconnecting(reason),
        };
        let refusal = outcome.refusal();
        if !status.report(outcome) {
            return;
        }
        match pause(backoff, refusal, &mut commands, &mut watched).await {
            Pause::Shutdown => return,
            Pause::Elapsed => backoff = (backoff * 2).min(MAX_BACKOFF),
            Pause::Reconnect => backoff = MIN_BACKOFF,
        }
        if events.is_closed() {
            return;
        }
    }
}

/// Why a connection couldn't be opened.
enum Refused {
    Version(u32),
    NotRunning(io::ErrorKind),
    Other(String),
}

impl Refused {
    fn reason(self) -> String {
        match self {
            Refused::Version(theirs) => format!("crewd speaks protocol {theirs}."),
            Refused::NotRunning(kind) => format!("crewd isn't running ({kind})."),
            Refused::Other(reason) => reason,
        }
    }
}

/// Connects and says `Hello`.
async fn open(socket: &Path) -> Result<UnixStream, Refused> {
    match tokio::time::timeout(PROBE_TIMEOUT, connection::hello(socket)).await {
        Ok(Ok(Some((stream, Ok(()))))) => Ok(stream),
        Ok(Ok(Some((_, Err(refusal))))) => match connection::their_version(&refusal) {
            Some(theirs) => Err(Refused::Version(theirs)),
            None => Err(Refused::Other(refusal)),
        },
        Ok(Ok(None)) => Err(Refused::Other("crewd closed the connection.".into())),
        Ok(Err(e))
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(Refused::NotRunning(e.kind()))
        }
        Ok(Err(e)) => Err(Refused::Other(e.to_string())),
        Err(_) => Err(Refused::Other("crewd didn't answer.".into())),
    }
}

enum Ended {
    /// Every handle or the event receiver is gone.
    Shutdown,
    /// Connected, then the control connection dropped, or the stream
    /// couldn't be reopened.
    Lost(String),
    /// Couldn't connect.
    Refused(Refused),
}

/// Which connection a frame came from. Each stream gets a new number, so
/// frames from a stream already replaced are told apart.
#[derive(Clone, Copy, PartialEq)]
enum Side {
    Control,
    Stream(u64),
}

type Frame = (Side, io::Result<Option<ServerMsg>>);

/// Aborts a reader task when dropped, closing its half.
struct Reader(JoinHandle<()>);

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The stream: its write half and its reader.
struct Stream {
    side: Side,
    wr: OwnedWriteHalf,
    _reader: Reader,
    /// Terminal sessions asked for and not answered yet, by request id.
    terminal_asks: HashMap<u64, String>,
    /// Watched terminal sessions crewd refused, as when it has not found
    /// them yet after starting. Asked for again once crewd lists them.
    refused: HashSet<String>,
}

impl Stream {
    /// Sends `req`, noting which terminal session it subscribes to.
    async fn send(&mut self, next_id: &mut u64, req: Request) -> io::Result<()> {
        if let Request::SubscribeTerminal { session } = &req {
            self.terminal_asks.insert(*next_id, session.clone());
        }
        send(&mut self.wr, next_id, req).await
    }
}

/// Opens a stream and subscribes to everything, each watched thread and
/// each watched terminal session.
async fn open_stream(
    socket: &Path,
    number: u64,
    frames: &mpsc::UnboundedSender<Frame>,
    watched: &Watched,
    next_id: &mut u64,
) -> Result<Stream, Refused> {
    let (rd, wr) = open(socket).await?.into_split();
    let side = Side::Stream(number);
    let reader = Reader(tokio::spawn(read_into(rd, side, frames.clone())));
    let mut stream = Stream {
        side,
        wr,
        _reader: reader,
        terminal_asks: HashMap::new(),
        refused: HashSet::new(),
    };
    for req in watched.requests() {
        stream
            .send(next_id, req)
            .await
            .map_err(|e| Refused::Other(e.to_string()))?;
    }
    Ok(stream)
}

/// Opens the control connection and a stream, then forwards notices and
/// answers requests until the control connection drops. A stream that
/// drops or falls behind (`Resync`) is reopened right away, or after
/// [`MIN_BACKOFF`] if it was reopened less than [`MAX_BACKOFF`] ago.
/// Requests in flight keep waiting for their answers.
async fn session(
    socket: &Path,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    status: &mut Status<'_>,
    watched: &mut Watched,
) -> Ended {
    let control = match open(socket).await {
        Ok(control) => control,
        Err(refused) => return Ended::Refused(refused),
    };
    let (control_rd, mut control_wr) = control.into_split();
    // `read_frame` can't be cancelled inside `select!`, a channel can.
    let (frames_tx, mut frames) = mpsc::unbounded_channel::<Frame>();
    let _control_reader = Reader(tokio::spawn(read_into(
        control_rd,
        Side::Control,
        frames_tx.clone(),
    )));
    let mut next_id = 1;
    let mut streams = 0;
    let mut reopened: Option<tokio::time::Instant> = None;
    let mut stream = match open_stream(socket, streams, &frames_tx, watched, &mut next_id).await {
        Ok(stream) => stream,
        Err(refused) => return Ended::Refused(refused),
    };
    if !status.report(ClientEvent::Connected) {
        return Ended::Shutdown;
    }
    let mut waiting: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();

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
                Some(Command::Reconnect) => {}
                Some(command) => {
                    if let Some(req) = watched.take(command) {
                        if let Request::UnsubscribeTerminal { session } = &req {
                            stream.refused.remove(session);
                        }
                        // A failed write shows up as the stream ending.
                        let _ = stream.send(&mut next_id, req).await;
                    }
                }
            },
            Some((side, frame)) = frames.recv() => {
                let stream_lost = match (side, frame) {
                    (Side::Control, Ok(Some(ServerMsg::Response { id, result }))) => {
                        if let Some(reply) = waiting.remove(&id) {
                            let _ = reply.send(result);
                        }
                        None
                    }
                    (Side::Control, Ok(Some(_))) => None,
                    (Side::Control, Ok(None)) => {
                        return Ended::Lost("crewd closed the connection.".into());
                    }
                    (Side::Control, Err(e)) => return Ended::Lost(e.to_string()),
                    // From a stream already replaced.
                    (side, _) if side != stream.side => None,
                    (_, Ok(Some(ServerMsg::Response { id, result }))) => {
                        let session = stream.terminal_asks.remove(&id);
                        if let Err(e) = result {
                            eprintln!("crew: crewd refused a subscription: {e}");
                            if let Some(session) = session
                                && watched.terminals.contains(&session)
                            {
                                stream.refused.insert(session);
                            }
                        }
                        None
                    }
                    (_, Ok(Some(ServerMsg::TerminalSessionChanged { session })))
                        if stream.refused.contains(&session.id) =>
                    {
                        let id = session.id.clone();
                        stream.refused.remove(&id);
                        if !status.send(ClientEvent::Msg(ServerMsg::TerminalSessionChanged { session })) {
                            return Ended::Shutdown;
                        }
                        let req = Request::SubscribeTerminal { session: id };
                        let _ = stream.send(&mut next_id, req).await;
                        None
                    }
                    (_, Ok(Some(ServerMsg::Resync))) => Some("crewd asked to catch up.".to_string()),
                    (_, Ok(Some(ServerMsg::TerminalSessionReset { session })))
                        if watched.terminals.contains(&session) =>
                    {
                        let reset = ServerMsg::TerminalSessionReset {
                            session: session.clone(),
                        };
                        if !status.send(ClientEvent::Msg(reset)) {
                            return Ended::Shutdown;
                        }
                        // Its new backlog follows the reset.
                        let req = Request::SubscribeTerminal { session };
                        let _ = stream.send(&mut next_id, req).await;
                        None
                    }
                    (_, Ok(Some(msg))) => {
                        if !status.send(ClientEvent::Msg(msg)) {
                            return Ended::Shutdown;
                        }
                        None
                    }
                    (_, Ok(None)) => Some("crewd closed the stream.".to_string()),
                    (_, Err(e)) => Some(e.to_string()),
                };
                if let Some(reason) = stream_lost {
                    eprintln!("crew: reopening the stream: {reason}");
                    // A stream that keeps falling behind would spin.
                    if reopened.is_some_and(|at| at.elapsed() < MAX_BACKOFF) {
                        tokio::time::sleep(MIN_BACKOFF).await;
                    }
                    reopened = Some(tokio::time::Instant::now());
                    streams += 1;
                    stream = match open_stream(socket, streams, &frames_tx, watched, &mut next_id).await {
                        Ok(stream) => stream,
                        Err(refused) => return Ended::Lost(refused.reason()),
                    };
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
/// right away with `refusal`; watches are remembered for the next
/// connection.
async fn pause(
    time: Duration,
    refusal: &'static str,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    watched: &mut Watched,
) -> Pause {
    let sleep = tokio::time::sleep(time);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return Pause::Elapsed,
            command = commands.recv() => match command {
                None => return Pause::Shutdown,
                Some(Command::Request(_, reply)) => {
                    let _ = reply.send(Err(refusal.into()));
                }
                Some(Command::Reconnect) => return Pause::Reconnect,
                Some(command) => {
                    let _ = watched.take(command);
                }
            },
        }
    }
}
