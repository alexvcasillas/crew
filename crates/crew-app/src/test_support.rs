//! Test helpers: a real crewd in-process and a fake one that refuses
//! `Hello`.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use crew_proto::{ClientMsg, PROTO_VERSION, ServerMsg, read_frame, write_frame};

/// A temp Crew home with a short path, so its socket stays under the Unix
/// path limit.
pub fn short_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("crew")
        .tempdir_in("/tmp")
        .unwrap()
}

/// crewd serving `home` on its own tokio runtime thread, stopped and
/// joined on drop.
pub struct Daemon {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Starts crewd and waits until its socket accepts. Agents can't run:
    /// there is no `claude` for them.
    pub fn start(home: PathBuf) -> Self {
        Self::start_with(home, PathBuf::from("/nonexistent/claude"))
    }

    /// Like [`Daemon::start`], running agents with the fake `claude_bin`.
    pub fn start_with(home: PathBuf, claude_bin: PathBuf) -> Self {
        Self::start_with_options(
            home,
            crewd::ServeOptions {
                claude_bin: Some(claude_bin),
                ..Default::default()
            },
        )
    }

    /// Like [`Daemon::start_with`], with more settings. crew-mcp is always
    /// off, and Codex too unless `opts` names a fake one.
    pub fn start_with_options(home: PathBuf, opts: crewd::ServeOptions) -> Self {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serve_home = home.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let opts = crewd::ServeOptions {
                    // Never the real codex.
                    codex_bin: opts
                        .codex_bin
                        .clone()
                        .or_else(|| Some(PathBuf::from("/nonexistent/codex"))),
                    crew_mcp_bin: None,
                    ..opts
                };
                let shutdown = async move {
                    let _ = stopped.await;
                };
                crewd::serve_until(&serve_home, opts, shutdown)
                    .await
                    .unwrap();
            });
        });
        let socket = home.join("crewd.sock");
        for _ in 0..500 {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return Self {
                    stop: Some(stop),
                    thread: Some(thread),
                };
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("crewd did not start");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Listens on `socket` and answers one client's `Hello` the way a crewd
/// speaking protocol `theirs` does. Join it after the client is done.
pub fn refuse_hello(socket: &Path, theirs: u32) -> JoinHandle<()> {
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let Some(ClientMsg::Request { id, .. }) = read_frame(&mut stream).await.unwrap() else {
                panic!("no hello");
            };
            let refused = ServerMsg::Response {
                id,
                result: Err(format!(
                    "crewd speaks protocol {theirs}, client speaks {PROTO_VERSION}. Restart crewd."
                )),
            };
            write_frame(&mut stream, &refused).await.unwrap();
        });
    })
}

/// Like [`refuse_hello`], for every client, for as long as the test
/// process runs. Used by `tests/client.rs`.
#[allow(dead_code)]
pub fn always_refuse_hello(socket: &Path, theirs: u32) {
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let Ok(Some(ClientMsg::Request { id, .. })) = read_frame(&mut stream).await
                    else {
                        return;
                    };
                    let refused = ServerMsg::Response {
                        id,
                        result: Err(format!(
                            "crewd speaks protocol {theirs}, client speaks {PROTO_VERSION}. Restart crewd."
                        )),
                    };
                    let _ = write_frame(&mut stream, &refused).await;
                });
            }
        });
    });
}

/// A recorded claude session from crew-core's fixtures, such as
/// `read_file.jsonl`.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../crew-core/tests/fixtures/claude")
        .join(name)
}

/// A fake `claude` in `dir` that runs the shell script `run` as an agent
/// and answers `--version` at once.
pub fn fake_claude(dir: &Path, run: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("claude.sh");
    let body = format!(
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo '9.9.9 (Claude Code)'; exit 0;;\n  auth) exit 0;;\nesac\n{run}\n"
    );
    std::fs::write(&bin, body).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The scripted stand-in for `codex app-server` in crew-core's tests.
pub fn fake_app_server() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../crew-core/tests/fake/codex_app_server.py")
}

/// A fake `codex` in `dir`. `login status` exits with `login_exit`. An
/// app-server started in `run_dir` (a run) plays `run-<n>.json` for the
/// n-th run; one started anywhere else (crewd listing models) plays
/// `models.json`. Scenarios are those of [`fake_app_server`].
pub fn fake_codex(dir: &Path, run_dir: &Path, login_exit: i32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("codex");
    let d = dir.display();
    let run_dir = run_dir.canonicalize().unwrap();
    let body = format!(
        "#!/bin/sh
         case \"$1\" in
         --version) echo 'codex-cli 0.165.0' ;;
         login) exit {login_exit} ;;
         app-server)
           here=$(pwd -P)
           if [ \"$here\" = '{run}' ]; then
             n=$(cat '{d}/runs' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{d}/runs'
             exec python3 '{fake}' '{d}/run-'$n'.json'
           fi
           exec python3 '{fake}' '{d}/models.json' ;;
         *) exit 2 ;;
         esac
",
        run = run_dir.display(),
        fake = fake_app_server().display(),
    );
    std::fs::write(&bin, body).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Forwards every client on `front` to crewd on `back`, keeping each
/// request the clients send, for as long as the test process runs.
pub fn recording_proxy(
    front: &Path,
    back: PathBuf,
) -> std::sync::Arc<std::sync::Mutex<Vec<crew_proto::Request>>> {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = std::os::unix::net::UnixListener::bind(front).unwrap();
    listener.set_nonblocking(true).unwrap();
    let kept = seen.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = tokio::net::UnixStream::connect(&back).await else {
                    continue;
                };
                let (mut client_rd, mut client_wr) = client.into_split();
                let (mut server_rd, mut server_wr) = server.into_split();
                let kept = kept.clone();
                tokio::spawn(async move {
                    while let Ok(Some(msg)) = read_frame::<_, ClientMsg>(&mut client_rd).await {
                        let ClientMsg::Request { req, .. } = &msg;
                        kept.lock().unwrap().push(req.clone());
                        if write_frame(&mut server_wr, &msg).await.is_err() {
                            break;
                        }
                    }
                });
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut server_rd, &mut client_wr).await;
                });
            }
        });
    });
    seen
}

/// Forwards every client on `front` to crewd on `back`, except that while
/// `failing` is set, `ChannelOverview` is answered with an error here.
/// Counts the `ChannelOverview` requests clients send.
pub fn failing_overview_proxy(
    front: &Path,
    back: PathBuf,
) -> (
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let failing = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let listener = std::os::unix::net::UnixListener::bind(front).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (fail, count) = (failing.clone(), asked.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = tokio::net::UnixStream::connect(&back).await else {
                    continue;
                };
                let (mut client_rd, client_wr) = client.into_split();
                let (mut server_rd, mut server_wr) = server.into_split();
                let client_wr = Arc::new(tokio::sync::Mutex::new(client_wr));
                let (fail, count, to_client) = (fail.clone(), count.clone(), client_wr.clone());
                tokio::spawn(async move {
                    while let Ok(Some(msg)) = read_frame::<_, ClientMsg>(&mut client_rd).await {
                        let ClientMsg::Request { id, req } = &msg;
                        if matches!(req, crew_proto::Request::ChannelOverview { .. }) {
                            count.fetch_add(1, Ordering::SeqCst);
                            if fail.load(Ordering::SeqCst) {
                                let refused = ServerMsg::Response {
                                    id: *id,
                                    result: Err("crewd is having a bad day.".into()),
                                };
                                let mut out = to_client.lock().await;
                                if write_frame(&mut *out, &refused).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                        }
                        if write_frame(&mut server_wr, &msg).await.is_err() {
                            break;
                        }
                    }
                });
                tokio::spawn(async move {
                    while let Ok(Some(msg)) = read_frame::<_, ServerMsg>(&mut server_rd).await {
                        let mut out = client_wr.lock().await;
                        if write_frame(&mut *out, &msg).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
    });
    (failing, asked)
}
