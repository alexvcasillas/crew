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
    /// Starts crewd and waits until its socket accepts.
    pub fn start(home: PathBuf) -> Self {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serve_home = home.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let opts = crewd::ServeOptions {
                    crew_mcp_bin: None,
                    ..Default::default()
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
