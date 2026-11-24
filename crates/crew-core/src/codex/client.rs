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

/// Id of a request the server sent us. Codex uses numbers, JSON-RPC allows strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

/// A message the server sent on its own.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification(ServerNotification),
    /// Needs an answer through [`CodexConn::reply`] with the same id.
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
    /// Starts the process with piped stdin and stdout. Stderr is left as the
    /// command has it. Needs a Tokio runtime.
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
        ))
    }

    /// A connection over any byte streams, for tests and other transports.
    pub fn from_io(
        reader: impl AsyncRead + Send + 'static,
        writer: impl AsyncWrite + Send + 'static,
    ) -> (CodexConn, mpsc::UnboundedReceiver<Incoming>) {
        Self::start(reader, writer, None, None, None)
    }

    fn start(
        reader: impl AsyncRead + Send + 'static,
        writer: impl AsyncWrite + Send + 'static,
        exited: Option<oneshot::Receiver<()>>,
        pid: Option<u32>,
        kill: Option<oneshot::Sender<()>>,
    ) -> (CodexConn, mpsc::UnboundedReceiver<Incoming>) {
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(Box::pin(reader), pending.clone(), tx, exited));
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
        let msg = json!({ "id": id, "method": method, "params": params });
        if let Err(e) = self.send(&msg).await {
            if let Some(map) = self.inner.pending.lock().unwrap().as_mut() {
                map.remove(&id);
            }
            return Err(e);
        }
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

async fn read_loop(
    reader: Pin<Box<dyn AsyncRead + Send>>,
    pending: Pending,
    tx: mpsc::UnboundedSender<Incoming>,
    exited: Option<oneshot::Receiver<()>>,
) {
    let mut lines = BufReader::new(reader).lines();
    let mut exited = exited;
    let mut gone = false;
    loop {
        let line = if gone {
            match tokio::time::timeout(DRAIN_AFTER_EXIT, lines.next_line()).await {
                Ok(line) => line,
                Err(_) => break,
            }
        } else if let Some(exit) = exited.as_mut() {
            tokio::select! {
                line = lines.next_line() => line,
                _ = exit => {
                    gone = true;
                    continue;
                }
            }
        } else {
            lines.next_line().await
        };
        let Ok(Some(line)) = line else { break };
        handle_line(&line, &pending, &tx);
    }
    // Fail everything still waiting; dropping `tx` ends the receiver.
    let waiting = pending.lock().unwrap().take();
    for (_, reply) in waiting.into_iter().flatten() {
        let _ = reply.send(Err(CodexError::Closed));
    }
}

fn handle_line(line: &str, pending: &Pending, tx: &mpsc::UnboundedSender<Incoming>) {
    let Ok(Value::Object(mut msg)) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let id = msg.get("id").filter(|id| !id.is_null()).cloned();
    if let Some(Value::String(method)) = msg.get("method").cloned() {
        let incoming = match id.map(serde_json::from_value::<RequestId>) {
            Some(Ok(id)) => Incoming::ServerRequest {
                id,
                method,
                params: msg.remove("params").unwrap_or(Value::Null),
            },
            Some(Err(_)) => return,
            None => Incoming::Notification(ServerNotification::parse(Value::Object(msg))),
        };
        let _ = tx.send(incoming);
        return;
    }
    let Some(id) = id.and_then(|id| id.as_i64()) else {
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
