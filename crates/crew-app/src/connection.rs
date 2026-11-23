//! Where crewd lives, whether it answers, and starting it.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use tokio::net::UnixStream;

/// The socket's file name inside the Crew home.
const SOCKET_NAME: &str = "crewd.sock";
/// How long a probe waits for crewd to answer `Hello`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Where the app finds crewd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionConfig {
    pub crew_home: PathBuf,
}

impl ConnectionConfig {
    /// `CREW_HOME`, else `~/.crew`.
    pub fn from_env() -> Self {
        Self::resolve(std::env::var_os("CREW_HOME"), std::env::var_os("HOME"))
    }

    fn resolve(crew_home: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Self {
        let crew_home = match crew_home.filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(home.unwrap_or_default()).join(".crew"),
        };
        Self { crew_home }
    }

    pub fn socket(&self) -> PathBuf {
        self.crew_home.join(SOCKET_NAME)
    }

    /// Where a crewd the app starts writes its output.
    pub fn log(&self) -> PathBuf {
        self.crew_home.join("crewd.log")
    }
}

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    NotRunning,
    /// crewd answered but refused this app's protocol version.
    VersionMismatch {
        theirs: Option<u32>,
        ours: u32,
    },
}

/// Connects to crewd and says `Hello`. Blocks for up to a few seconds, so
/// run it off the UI thread.
pub fn probe(conn: &ConnectionConfig) -> ConnectionState {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return ConnectionState::NotRunning,
    };
    let socket = conn.socket();
    runtime.block_on(async move {
        tokio::time::timeout(PROBE_TIMEOUT, hello(&socket))
            .await
            .unwrap_or(Ok(None))
            .map_or(ConnectionState::NotRunning, |answer| match answer {
                Some(Ok(())) => ConnectionState::Connected,
                Some(Err(_)) => ConnectionState::VersionMismatch {
                    theirs: None,
                    ours: PROTO_VERSION,
                },
                None => ConnectionState::NotRunning,
            })
    })
}

/// crewd's answer to `Hello`, or `None` if it hung up first.
async fn hello(socket: &Path) -> io::Result<Option<Result<(), String>>> {
    let mut stream = UnixStream::connect(socket).await?;
    let hello = ClientMsg::Request {
        id: 1,
        req: Request::Hello {
            version: PROTO_VERSION,
        },
    };
    write_frame(&mut stream, &hello).await?;
    loop {
        match read_frame::<_, ServerMsg>(&mut stream).await? {
            Some(ServerMsg::Response { id: 1, result }) => return Ok(Some(result.map(|_| ()))),
            Some(_) => continue,
            None => return Ok(None),
        }
    }
}

/// Why crewd couldn't be started.
#[derive(Debug)]
pub enum StartError {
    /// No crewd binary next to the app's own.
    NotFound,
    Io(io::Error),
}

/// The crewd binary shipped next to the app's executable `exe`.
pub fn crewd_beside(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.join("crewd"))
}

/// Starts the crewd next to the app as a detached process on `conn`'s
/// home, its output going to `crewd.log` there. Doesn't wait for it to
/// listen: probe for that.
pub fn start_crewd(conn: &ConnectionConfig) -> Result<(), StartError> {
    let exe = std::env::current_exe().map_err(StartError::Io)?;
    let bin = crewd_beside(&exe)
        .filter(|b| b.is_file())
        .ok_or(StartError::NotFound)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&conn.crew_home)
        .map_err(StartError::Io)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(conn.log())
        .map_err(StartError::Io)?;
    let mut child = Command::new(bin)
        .env("CREW_HOME", &conn.crew_home)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(StartError::Io)?)
        .stderr(log)
        // Its own process group, so a Ctrl-C meant for the app spares it.
        .process_group(0)
        .spawn()
        .map_err(StartError::Io)?;
    // Reaps it if it exits while the app runs. It outlives the app.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Asks the crewd answering on `conn`'s socket to stop (SIGTERM) and waits
/// up to `wait` for it to stop answering. Returns whether it stopped.
#[cfg(target_os = "macos")]
pub fn stop_crewd(conn: &ConnectionConfig, wait: Duration) -> bool {
    use nix::sys::signal::{Signal, kill};
    use nix::sys::socket::{getsockopt, sockopt::LocalPeerPid};
    use nix::unistd::Pid;

    let socket = conn.socket();
    let Ok(stream) = std::os::unix::net::UnixStream::connect(&socket) else {
        return true;
    };
    if let Ok(pid) = getsockopt(&stream, LocalPeerPid) {
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
    drop(stream);
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < wait {
        if std::os::unix::net::UnixStream::connect(&socket).is_err() {
            return true;
        }
        std::thread::sleep(step);
        waited += step;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crew_home_comes_from_the_environment_else_home() {
        let conn = ConnectionConfig::resolve(Some("/work/crew".into()), Some("/work/me".into()));
        assert_eq!(conn.crew_home, PathBuf::from("/work/crew"));
        let conn = ConnectionConfig::resolve(None, Some("/work/me".into()));
        assert_eq!(conn.crew_home, PathBuf::from("/work/me/.crew"));
        let conn = ConnectionConfig::resolve(Some("".into()), Some("/work/me".into()));
        assert_eq!(conn.crew_home, PathBuf::from("/work/me/.crew"));
        assert_eq!(conn.socket(), PathBuf::from("/work/me/.crew/crewd.sock"));
        assert_eq!(conn.log(), PathBuf::from("/work/me/.crew/crewd.log"));
    }

    #[test]
    fn crewd_is_looked_for_next_to_the_app() {
        assert_eq!(
            crewd_beside(Path::new("/Applications/Crew.app/Contents/MacOS/Crew")),
            Some(PathBuf::from("/Applications/Crew.app/Contents/MacOS/crewd"))
        );
    }

    #[test]
    fn probe_without_a_socket_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let conn = ConnectionConfig {
            crew_home: dir.path().to_path_buf(),
        };
        assert_eq!(probe(&conn), ConnectionState::NotRunning);
    }

    #[test]
    fn probe_treats_a_refused_hello_as_a_version_mismatch() {
        let dir = tempfile::Builder::new()
            .prefix("crew")
            .tempdir_in("/tmp")
            .unwrap();
        let conn = ConnectionConfig {
            crew_home: dir.path().to_path_buf(),
        };
        let listener = std::os::unix::net::UnixListener::bind(conn.socket()).unwrap();
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .unwrap();
            rt.block_on(async move {
                listener.set_nonblocking(true).unwrap();
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some(ClientMsg::Request { id, .. }) = read_frame(&mut stream).await.unwrap()
                else {
                    panic!("no hello");
                };
                let refused = ServerMsg::Response {
                    id,
                    result: Err("crewd speaks protocol 99, client speaks 3. Restart crewd.".into()),
                };
                write_frame(&mut stream, &refused).await.unwrap();
            });
        });
        assert_eq!(
            probe(&conn),
            ConnectionState::VersionMismatch {
                theirs: None,
                ours: PROTO_VERSION
            }
        );
        server.join().unwrap();
    }
}
