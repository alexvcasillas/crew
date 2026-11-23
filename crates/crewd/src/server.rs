//! Unix socket server: framing, handshake and the per-connection loop.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use crew_core::ids::ThreadId;
use crew_core::store::Store;
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use crate::service::Service;

/// Socket file name inside the crew home.
pub const SOCKET_NAME: &str = "crewd.sock";
/// Lock file name inside the crew home.
pub const LOCK_NAME: &str = "crewd.lock";
/// Database file name inside the crew home.
pub const DB_NAME: &str = "crew.db";

/// Settings for [`serve`].
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Most runs that may work at once.
    pub max_runs: usize,
    /// Overrides the `claude` binary for every run.
    pub claude_bin: Option<PathBuf>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            max_runs: 4,
            claude_bin: None,
        }
    }
}

fn log(msg: impl std::fmt::Display) {
    eprintln!("crewd: {msg}");
}

/// Serves clients on `home/crewd.sock`. Runs until accepting fails for good.
pub async fn serve(home: &Path, opts: ServeOptions) -> anyhow::Result<()> {
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
        bail!("crewd is already running on {}", sock.display());
    }
    let _lock = lock_home(home, &sock)?;

    let store = Store::open(&home.join(DB_NAME)).context("opening the database")?;
    let interrupted = store.mark_working_runs_interrupted()?;
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

    let service = Service::new(store, opts.max_runs, opts.claude_bin);
    loop {
        match listener.accept().await {
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
}

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
        Err((_, Errno::EWOULDBLOCK)) => bail!("crewd is already running on {}", sock.display()),
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

async fn connection(service: Service, stream: UnixStream) -> io::Result<()> {
    let uid = match stream.peer_cred() {
        Ok(cred) => cred.uid(),
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

    let notices = service.notices();
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
    let result = requests_and_notices(&service, &mut wr, req_rx, notices).await;
    reader.abort();
    result
}

/// Answers requests and forwards notices for subscribed threads until the
/// client goes away. Runs keep going after that.
async fn requests_and_notices(
    service: &Service,
    wr: &mut OwnedWriteHalf,
    mut req_rx: mpsc::Receiver<ClientMsg>,
    mut notices: broadcast::Receiver<crate::service::Notice>,
) -> io::Result<()> {
    // Thread -> serial already covered by that thread's backlog.
    let mut subscribed: HashMap<ThreadId, u64> = HashMap::new();
    loop {
        tokio::select! {
            msg = req_rx.recv() => {
                let Some(ClientMsg::Request { id, req }) = msg else {
                    return Ok(());
                };
                match req {
                    Request::Subscribe { thread } => match service.subscribe(thread) {
                        Ok(backlog) => {
                            respond(wr, id, Ok(serde_json::Value::Null)).await?;
                            for msg in &backlog.msgs {
                                write_frame(wr, msg).await?;
                            }
                            subscribed.insert(thread, backlog.serial);
                        }
                        Err(e) => respond(wr, id, Err(e.to_string())).await?,
                    },
                    Request::Hello { .. } => {
                        respond(wr, id, Err("Hello was already sent.".into())).await?;
                    }
                    other => {
                        let result = service.handle(other);
                        respond(wr, id, result).await?;
                    }
                }
            }
            notice = notices.recv() => match notice {
                Ok(n) => {
                    if subscribed.get(&n.thread).is_some_and(|seen| n.serial > *seen) {
                        write_frame(wr, &n.msg).await?;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    log(format!("a client fell behind and missed {missed} notifications"));
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}
