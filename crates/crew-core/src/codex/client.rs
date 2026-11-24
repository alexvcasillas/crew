//! An async connection to `codex app-server` over newline-delimited JSON.

use std::collections::HashMap;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use super::proto::{InitializeParams, InitializeResponse, ServerNotification, method};

/// How long to keep reading stdout after the process exited, in case a
/// leftover child still holds the pipe open.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(500);

pub type Result<T> = std::result::Result<T, CodexError>;

#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    #[error("codex app-server is not running")]
    Closed,
    #[error("codex app-server error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("could not talk to codex app-server: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected reply from codex app-server: {0}")]
    Json(#[from] serde_json::Error),
}

/// Id of a request the server sent us. Codex uses numbers, JSON-RPC allows
/// strings; anything else is kept as is so the reply can echo it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    Str(String),
    Other(Value),
}

/// A message the server sent on its own.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification(ServerNotification),
    /// Needs an answer through [`CodexConn::reply`] or
    /// [`CodexConn::reply_error`] with the same id, or Codex waits for ever.
    /// Read it with [`super::proto::ServerRequest::parse`].
    ServerRequest {
        id: RequestId,
        method: String,
        params: Value,
    },
}

type Reply = oneshot::Sender<Result<Value>>;

/// Requests waiting for a response. `None` once the connection is gone.
type Pending = Arc<Mutex<Option<HashMap<i64, Reply>>>>;

type Writer = Pin<Box<dyn AsyncWrite + Send>>;

/// A connection to one app-server. Cheap to clone; the process is killed
/// when the last clone is dropped.
#[derive(Clone)]
pub struct CodexConn {
    inner: Arc<Inner>,
}

struct Inner {
    writer: tokio::sync::Mutex<Writer>,
    pending: Pending,
    next_id: AtomicI64,
    pid: Option<u32>,
    /// Dropping this tells the process watcher to kill the process.
    _kill: Option<oneshot::Sender<()>>,
}

impl CodexConn {
    /// Longest line read from the server. Longer lines are skipped.
    pub const MAX_LINE: usize = 64 * 1024 * 1024;

    /// Starts the process with piped stdin and stdout. Stderr is left as the
    /// command has it. Needs a Tokio runtime.
    ///
    /// The receiver is unbounded so a slow reader can never hold up
    /// responses; drain it.
    pub fn spawn(mut cmd: Command) -> Result<(CodexConn, mpsc::UnboundedReceiver<Incoming>)> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let pid = child.id();

        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let (exit_tx, exit_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = kill_rx => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
            let _ = exit_tx.send(());
        });

        Ok(Self::start(
            stdout,
            stdin,
            Some(exit_rx),
            pid,
            Some(kill_tx),
            Self::MAX_LINE,
        ))
    }

    /// A connection over any byte streams, for tests and other transports.
    pub fn from_io(
        reader: impl AsyncRead + Send + 'static,
        writer: impl AsyncWrite + Send + 'static,
    ) -> (CodexConn, mpsc::UnboundedReceiver<Incoming>) {
        Self::from_io_with_max_line(reader, writer, Self::MAX_LINE)
    }

    /// [`CodexConn::from_io`] with a different line cap.
    pub fn from_io_with_max_line(
        reader: impl AsyncRead + Send + 'static,
        writer: impl AsyncWrite + Send + 'static,
        max_line: usize,
    ) -> (CodexConn, mpsc::UnboundedReceiver<Incoming>) {
        Self::start(reader, writer, None, None, None, max_line)
    }

    fn start(
        reader: impl AsyncRead + Send + 'static,
        writer: impl AsyncWrite + Send + 'static,
        exited: Option<oneshot::Receiver<()>>,
        pid: Option<u32>,
        kill: Option<oneshot::Sender<()>>,
        max_line: usize,
    ) -> (CodexConn, mpsc::UnboundedReceiver<Incoming>) {
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (tx, rx) = mpsc::unbounded_channel();
        let lines = LineReader::new(Box::pin(reader), max_line);
        tokio::spawn(read_loop(lines, pending.clone(), tx, exited));
        let conn = CodexConn {
            inner: Arc::new(Inner {
                writer: tokio::sync::Mutex::new(Box::pin(writer)),
                pending,
                next_id: AtomicI64::new(1),
                pid,
                _kill: kill,
            }),
        };
        (conn, rx)
    }

    /// Process id when started with [`CodexConn::spawn`].
    pub fn pid(&self) -> Option<u32> {
        self.inner.pid
    }

    /// Sends a request and waits for its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        match self.inner.pending.lock().unwrap().as_mut() {
            Some(map) => map.insert(id, tx),
            None => return Err(CodexError::Closed),
        };
        // Removes the entry if this future is dropped or fails early.
        let _guard = PendingGuard {
            pending: &self.inner.pending,
            id,
        };
        let msg = json!({ "id": id, "method": method, "params": params });
        self.send(&msg).await?;
        rx.await.unwrap_or(Err(CodexError::Closed))
    }

    /// [`CodexConn::request`] with typed params and result.
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        let result = self.request(method, serde_json::to_value(params)?).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Sends a notification. `None` params leaves the field out.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let msg = match params {
            Some(params) => json!({ "method": method, "params": params }),
            None => json!({ "method": method }),
        };
        self.send(&msg).await
    }

    /// Answers a server request.
    pub async fn reply(&self, id: RequestId, result: Value) -> Result<()> {
        self.send(&json!({ "id": id, "result": result })).await
    }

    /// Answers a server request with an error, for requests Crew does not handle.
    pub async fn reply_error(&self, id: RequestId, code: i64, message: &str) -> Result<()> {
        let error = json!({ "code": code, "message": message });
        self.send(&json!({ "id": id, "error": error })).await
    }

    /// Runs the handshake: `initialize`, then the `initialized` notification.
    pub async fn initialize(&self, params: &InitializeParams) -> Result<InitializeResponse> {
        let resp = self.call(method::INITIALIZE, params).await?;
        self.notify(method::INITIALIZED, None).await?;
        Ok(resp)
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        if self.inner.pending.lock().unwrap().is_none() {
            return Err(CodexError::Closed);
        }
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        let mut w = self.inner.writer.lock().await;
        let written = async {
            w.write_all(&line).await?;
            w.flush().await
        }
        .await;
        written.map_err(|e| match e.kind() {
            std::io::ErrorKind::BrokenPipe => CodexError::Closed,
            _ => CodexError::Io(e),
        })
    }
}

struct PendingGuard<'a> {
    pending: &'a Pending,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Some(map) = self.pending.lock().unwrap().as_mut() {
            map.remove(&self.id);
        }
    }
}

enum Line {
    Full(Vec<u8>),
    TooLong,
    Eof,
}

/// Reads raw lines with a length cap. Over-long lines are dropped up to the
/// next newline. Cancel safe: partial lines live in `self`.
struct LineReader {
    inner: BufReader<Pin<Box<dyn AsyncRead + Send>>>,
    buf: Vec<u8>,
    discarding: bool,
    max: usize,
}

impl LineReader {
    fn new(reader: Pin<Box<dyn AsyncRead + Send>>, max: usize) -> Self {
        LineReader {
            inner: BufReader::new(reader),
            buf: Vec::new(),
            discarding: false,
            max,
        }
    }

    async fn next(&mut self) -> std::io::Result<Line> {
        loop {
            let avail = self.inner.fill_buf().await?;
            if avail.is_empty() {
                if std::mem::take(&mut self.discarding) {
                    return Ok(Line::TooLong);
                }
                if self.buf.is_empty() {
                    return Ok(Line::Eof);
                }
                return Ok(Line::Full(std::mem::take(&mut self.buf)));
            }
            let newline = avail.iter().position(|&b| b == b'\n');
            let chunk = &avail[..newline.unwrap_or(avail.len())];
            if !self.discarding {
                if self.buf.len() + chunk.len() > self.max {
                    self.discarding = true;
                    self.buf = Vec::new();
                } else {
                    self.buf.extend_from_slice(chunk);
                }
            }
            let used = newline.map_or(avail.len(), |i| i + 1);
            self.inner.consume(used);
            if newline.is_some() {
                if std::mem::take(&mut self.discarding) {
                    return Ok(Line::TooLong);
                }
                return Ok(Line::Full(std::mem::take(&mut self.buf)));
            }
        }
    }
}

async fn read_loop(
    mut lines: LineReader,
    pending: Pending,
    tx: mpsc::UnboundedSender<Incoming>,
    exited: Option<oneshot::Receiver<()>>,
) {
    let mut exited = exited;
    let mut gone = false;
    loop {
        let line = if gone {
            match tokio::time::timeout(DRAIN_AFTER_EXIT, lines.next()).await {
                Ok(line) => line,
                Err(_) => break,
            }
        } else if let Some(exit) = exited.as_mut() {
            tokio::select! {
                line = lines.next() => line,
                _ = exit => {
                    gone = true;
                    continue;
                }
            }
        } else {
            lines.next().await
        };
        match line {
            Ok(Line::Full(line)) => handle_line(&line, &pending, &tx),
            Ok(Line::TooLong) => eprintln!(
                "crew: skipped a codex app-server line longer than {} bytes",
                lines.max
            ),
            Ok(Line::Eof) => break,
            Err(e) => {
                eprintln!("crew: reading from codex app-server failed: {e}");
                break;
            }
        }
    }
    // Fail everything still waiting; dropping `tx` ends the receiver.
    let waiting = pending.lock().unwrap().take();
    for (_, reply) in waiting.into_iter().flatten() {
        let _ = reply.send(Err(CodexError::Closed));
    }
}

/// Parses a line, replacing bad UTF-8 if that is all that is wrong with it.
fn parse_line(line: &[u8]) -> Option<Value> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let parsed = serde_json::from_slice(line).or_else(|e| match std::str::from_utf8(line) {
        Ok(_) => Err(e),
        Err(_) => serde_json::from_str(&String::from_utf8_lossy(line)),
    });
    match parsed {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("crew: codex app-server sent a line that is not JSON: {e}");
            None
        }
    }
}

fn handle_line(line: &[u8], pending: &Pending, tx: &mpsc::UnboundedSender<Incoming>) {
    let Some(Value::Object(mut msg)) = parse_line(line) else {
        return;
    };
    let id = msg.get("id").filter(|id| !id.is_null()).cloned();
    if let Some(Value::String(method)) = msg.get("method").cloned() {
        let incoming = match id {
            Some(id) => Incoming::ServerRequest {
                id: serde_json::from_value(id.clone()).unwrap_or(RequestId::Other(id)),
                method,
                params: msg.remove("params").unwrap_or(Value::Null),
            },
            None => Incoming::Notification(ServerNotification::parse(Value::Object(msg))),
        };
        let _ = tx.send(incoming);
        return;
    }
    let Some(id) = id.as_ref().and_then(Value::as_i64) else {
        eprintln!("crew: codex app-server sent a reply with an unknown id: {id:?}");
        return;
    };
    let Some(reply) = pending.lock().unwrap().as_mut().and_then(|m| m.remove(&id)) else {
        return;
    };
    let result = match msg.remove("error") {
        Some(err) => Err(CodexError::Rpc {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            data: err.get("data").cloned(),
        }),
        None => Ok(msg.remove("result").unwrap_or(Value::Null)),
    };
    let _ = reply.send(result);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_len(conn: &CodexConn) -> usize {
        conn.inner.pending.lock().unwrap().as_ref().unwrap().len()
    }

    #[tokio::test]
    async fn dropped_request_leaves_no_pending_entry() {
        let (client_end, _server_end) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(client_end);
        let (conn, _rx) = CodexConn::from_io(read, write);

        let res = tokio::time::timeout(
            Duration::from_millis(50),
            conn.request("model/list", json!({})),
        )
        .await;
        assert!(res.is_err(), "no reply was sent, so it must time out");
        assert_eq!(pending_len(&conn), 0);
    }
}
