//! Where crewd lives, whether it answers, and starting it.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use tokio::net::UnixStream;

pub use crate::model::ConnectionState;

/// The socket's file name inside the Crew home.
const SOCKET_NAME: &str = "crewd.sock";
/// How long a probe waits for crewd to answer `Hello`.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

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
        match tokio::time::timeout(PROBE_TIMEOUT, hello(&socket)).await {
            Ok(Ok(Some((_, Ok(()))))) => ConnectionState::Connected,
            Ok(Ok(Some((_, Err(e))))) => match their_version(&e) {
                Some(theirs) => ConnectionState::VersionMismatch {
                    theirs,
                    ours: PROTO_VERSION,
                },
                None => {
                    eprintln!("crew: crewd refused hello: {e}");
                    ConnectionState::NotRunning
                }
            },
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => ConnectionState::NotRunning,
        }
    })
}

/// The version in crewd's refusal of `Hello`:
/// `crewd speaks protocol 2, client speaks 3. Restart crewd.`
pub(crate) fn their_version(refusal: &str) -> Option<u32> {
    let rest = refusal.strip_prefix("crewd speaks protocol ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Connects and says `Hello`. Returns the connection with crewd's answer,
/// or `None` if it hung up first.
pub(crate) async fn hello(socket: &Path) -> io::Result<Option<(UnixStream, Result<(), String>)>> {
    let mut stream = UnixStream::connect(socket).await?;
    let hello = ClientMsg::Request {
        id: 0,
        req: Request::Hello {
            version: PROTO_VERSION,
        },
    };
    write_frame(&mut stream, &hello).await?;
    loop {
        match read_frame::<_, ServerMsg>(&mut stream).await? {
            Some(ServerMsg::Response { id: 0, result }) => {
                return Ok(Some((stream, result.map(|_| ()))));
            }
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

/// How many processes the crewd answering on `conn`'s socket has
/// started, which are its running agents. 0 if none answers. Blocks, so
/// run it off the UI thread.
#[cfg(target_os = "macos")]
pub fn crewd_children(conn: &ConnectionConfig) -> usize {
    use nix::sys::socket::{getsockopt, sockopt::LocalPeerPid};

    let Ok(stream) = std::os::unix::net::UnixStream::connect(conn.socket()) else {
        return 0;
    };
    let Ok(pid) = getsockopt(&stream, LocalPeerPid) else {
        return 0;
    };
    drop(stream);
    match Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
    {
        Ok(out) => count_pids(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => 0,
    }
}

/// The number of pids `pgrep` printed, one a line.
#[cfg(target_os = "macos")]
fn count_pids(printed: &str) -> usize {
    printed
        .lines()
        .filter(|l| l.trim().parse::<u32>().is_ok())
        .count()
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
        let dir = crate::test_support::short_home();
        let conn = ConnectionConfig {
            crew_home: dir.path().to_path_buf(),
        };
        let server = crate::test_support::refuse_hello(&conn.socket(), 99);
        assert_eq!(
            probe(&conn),
            ConnectionState::VersionMismatch {
                theirs: 99,
                ours: PROTO_VERSION
            }
        );
        server.join().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pgrep_output_counts_one_agent_a_line() {
        assert_eq!(count_pids("812\n813\n"), 2);
        assert_eq!(count_pids(""), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nothing_on_the_socket_has_no_agents() {
        let dir = tempfile::tempdir().unwrap();
        let conn = ConnectionConfig {
            crew_home: dir.path().to_path_buf(),
        };
        assert_eq!(crewd_children(&conn), 0);
    }

    #[test]
    fn only_a_version_refusal_is_a_mismatch() {
        assert_eq!(
            their_version("crewd speaks protocol 2, client speaks 3. Restart crewd."),
            Some(2)
        );
        assert_eq!(their_version("Send Hello first."), None);
        assert_eq!(their_version("crewd speaks protocol x"), None);
    }
}
