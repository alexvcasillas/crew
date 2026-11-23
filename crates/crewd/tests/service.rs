use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::claude::parse_line;
use crew_core::event::{Event, RunStatus};
use crew_core::ids::{AgentId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::store::{Author, StoredMessage, StoredRun};
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(8);

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fake(name: &str) -> PathBuf {
    manifest_dir().join("tests/fake").join(name)
}

fn fixture(name: &str) -> PathBuf {
    manifest_dir()
        .join("../crew-core/tests/fixtures/claude")
        .join(name)
}

fn fixture_summary(name: &str) -> String {
    std::fs::read_to_string(fixture(name))
        .unwrap()
        .lines()
        .flat_map(parse_line)
        .filter_map(|e| match e {
            Event::TurnEnd { summary, .. } => Some(summary),
            _ => None,
        })
        .next_back()
        .unwrap()
}

/// Writes a script that replays `fixture` through the fake claude.
fn replay_script(dir: &Path, fixture_name: &str) -> PathBuf {
    let path = dir.join("claude-replay.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec '{}' '{}'\n",
            fake("claude_ok.sh").display(),
            fixture(fixture_name).display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Like `replay_script`, but also appends the arguments to `args.log`.
/// Each argument ends with byte 0x1e and each call with byte 0x1f.
fn logging_replay_script(dir: &Path, fixture_name: &str) -> (PathBuf, PathBuf) {
    let path = dir.join("claude-logging.sh");
    let log = dir.join("args.log");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\036' \"$a\" >> '{log}'; done\nprintf '\\037' >> '{log}'\nexec '{}' '{}'\n",
            fake("claude_ok.sh").display(),
            fixture(fixture_name).display(),
            log = log.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path, log)
}

/// Splits the args log into one argument list per call.
fn logged_calls(log: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(log).unwrap();
    text.split_terminator('\u{1f}')
        .map(|call| call.split_terminator('\u{1e}').map(String::from).collect())
        .collect()
}

fn arg_after(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

struct Daemon {
    home: TempDir,
}

impl Daemon {
    async fn start(max_runs: usize, claude_bin: PathBuf) -> Daemon {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().to_path_buf();
        tokio::spawn(async move {
            let opts = ServeOptions {
                max_runs,
                claude_bin: Some(claude_bin),
            };
            if let Err(e) = serve(&path, opts).await {
                panic!("serve failed: {e:#}");
            }
        });
        let sock = home.path().join("crewd.sock");
        timeout(WAIT, async {
            while UnixStream::connect(&sock).await.is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("crewd did not start");
        Daemon { home }
    }

    fn socket(&self) -> PathBuf {
        self.home.path().join("crewd.sock")
    }

    async fn client(&self) -> Client {
        let mut c = Client::raw(&self.socket()).await;
        c.call(Request::Hello {
            version: PROTO_VERSION,
        })
        .await
        .unwrap();
        c
    }
}

struct Client {
    stream: UnixStream,
    next_id: u64,
    /// Notifications read while waiting for a response.
    pending: Vec<ServerMsg>,
}

impl Client {
    async fn raw(sock: &Path) -> Client {
        Client {
            stream: UnixStream::connect(sock).await.unwrap(),
            next_id: 1,
            pending: Vec::new(),
        }
    }

    async fn send(&mut self, req: Request) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.stream, &ClientMsg::Request { id, req })
            .await
            .unwrap();
        id
    }

    async fn recv(&mut self) -> Option<ServerMsg> {
        if !self.pending.is_empty() {
            return Some(self.pending.remove(0));
        }
        timeout(WAIT, read_frame(&mut self.stream))
            .await
            .expect("timed out waiting for crewd")
            .unwrap()
    }

    async fn call(&mut self, req: Request) -> Result<serde_json::Value, String> {
        let id = self.send(req).await;
        loop {
            let msg = timeout(WAIT, read_frame(&mut self.stream))
                .await
                .expect("timed out waiting for a response")
                .unwrap()
                .expect("connection closed");
            match msg {
                ServerMsg::Response { id: got, result } if got == id => return result,
                other => self.pending.push(other),
            }
        }
    }

    async fn ok<T: serde::de::DeserializeOwned>(&mut self, req: Request) -> T {
        serde_json::from_value(self.call(req).await.unwrap()).unwrap()
    }

    /// Reads notifications until `done` returns true, and returns them all.
    async fn until(&mut self, mut done: impl FnMut(&ServerMsg) -> bool) -> Vec<ServerMsg> {
        let mut seen = Vec::new();
        loop {
            let msg = self.recv().await.expect("connection closed");
            let stop = done(&msg);
            seen.push(msg);
            if stop {
                return seen;
            }
        }
    }
}

fn forge() -> AgentSpec {
    AgentSpec {
        name: "Forge".into(),
        role: "Builds things".into(),
        runtime: Runtime::ClaudeCode,
        model: "claude-haiku-4-5".into(),
        instructions: String::new(),
        permission_mode: PermissionMode::EditFiles,
        allowed_tools: vec![],
    }
}

/// Creates a project rooted in `root`, a channel, the Forge agent and `n` threads.
async fn setup(c: &mut Client, root: &Path, n: usize) -> (AgentId, Vec<ThreadId>) {
    let project: ProjectId = c
        .ok(Request::CreateProject {
            name: "demo".into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
        })
        .await;
    let channel: ChannelId = c
        .ok(Request::CreateChannel {
            project,
            name: "general".into(),
        })
        .await;
    let agent: AgentId = c.ok(Request::CreateAgent { spec: forge() }).await;
    let mut threads = Vec::new();
    for i in 0..n {
        threads.push(
            c.ok(Request::CreateThread {
                channel,
                title: format!("thread {i}"),
            })
            .await,
        );
    }
    (agent, threads)
}

fn is_agent_message(m: &ServerMsg) -> bool {
    matches!(
        m,
        ServerMsg::Message {
            message: StoredMessage {
                author: Author::Agent(_),
                ..
            },
            ..
        }
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_with_wrong_version_is_refused() {
    let d = Daemon::start(4, fake("claude_sleeps.sh")).await;
    let mut c = Client::raw(&d.socket()).await;
    let err = c.call(Request::Hello { version: 99 }).await.unwrap_err();
    assert_eq!(
        err,
        "crewd speaks protocol 1, client speaks 99. Restart crewd."
    );
    assert!(c.recv().await.is_none(), "connection should be closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn mention_starts_run_and_agent_replies() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(4, replay_script(dir.path(), "edit_and_test.jsonl")).await;
    let mut c = d.client().await;
    let (agent, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    assert_eq!(
        c.call(Request::Subscribe { thread }).await.unwrap(),
        serde_json::Value::Null
    );

    let _msg_id: crew_core::ids::MessageId = c
        .ok(Request::PostMessage {
            thread,
            body: "@Forge fix it".into(),
        })
        .await;

    let seen = c
        .until(|m| {
            matches!(
                m,
                ServerMsg::RunStatus {
                    status: RunStatus::Done,
                    ..
                }
            )
        })
        .await;

    let working = seen
        .iter()
        .position(|m| {
            matches!(
                m,
                ServerMsg::RunStatus {
                    status: RunStatus::Working,
                    ..
                }
            )
        })
        .expect("RunStatus Working");
    let seqs: Vec<u64> = seen
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Event { seq, thread: t, .. } if *t == thread => Some(*seq),
            _ => None,
        })
        .collect();
    assert!(!seqs.is_empty(), "no events");
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    let first_event = seen
        .iter()
        .position(|m| matches!(m, ServerMsg::Event { .. }))
        .unwrap();
    assert!(working < first_event);

    let your_message = seen.iter().any(|m| matches!(m, ServerMsg::Message { message, .. } if message.author == Author::You && message.body == "@Forge fix it"));
    assert!(your_message);

    let reply = seen
        .iter()
        .find_map(|m| match m {
            ServerMsg::Message { message, .. } if message.author == Author::Agent(agent) => {
                Some(message.body.clone())
            }
            _ => None,
        })
        .expect("agent reply before Done");
    assert_eq!(reply, fixture_summary("edit_and_test.jsonl"));

    let runs: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Done),
        })
        .await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].thread, thread);
    assert_eq!(runs[0].agent, agent);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_mention_posts_system_message() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(4, fake("claude_sleeps.sh")).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "hey @Nobody".into(),
    })
    .await
    .unwrap();
    let seen = c
        .until(
            |m| matches!(m, ServerMsg::Message { message, .. } if message.author == Author::System),
        )
        .await;
    let ServerMsg::Message { message, .. } = seen.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(message.body, "No agent named @Nobody.");
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert!(runs.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn late_subscriber_gets_backlog_from_seq_0() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(4, replay_script(dir.path(), "edit_and_test.jsonl")).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge fix it".into(),
    })
    .await
    .unwrap();
    let live = c
        .until(|m| {
            matches!(
                m,
                ServerMsg::RunStatus {
                    status: RunStatus::Done,
                    ..
                }
            )
        })
        .await;
    let live_events: Vec<(u64, Event)> = live
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::Event { seq, event, .. } => Some((seq, event)),
            _ => None,
        })
        .collect();

    let mut late = d.client().await;
    late.call(Request::Subscribe { thread }).await.unwrap();
    let backlog = late.until(is_agent_message).await;
    let backlog_events: Vec<(u64, Event)> = backlog
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Event { seq, event, .. } => Some((*seq, event.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(backlog_events.first().map(|e| e.0), Some(0));
    assert_eq!(backlog_events, live_events);
    let bodies: Vec<&str> = backlog
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Message { message, .. } => Some(message.body.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(bodies, vec!["@Forge fix it", "Fixed."]);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_keeps_run_alive() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(4, replay_script(dir.path(), "edit_and_test.jsonl")).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::PostMessage {
        thread,
        body: "@Forge fix it".into(),
    })
    .await
    .unwrap();
    drop(c);

    let mut other = d.client().await;
    other.call(Request::Subscribe { thread }).await.unwrap();
    let seen = other.until(is_agent_message).await;
    let ServerMsg::Message { message, .. } = seen.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(message.body, "Fixed.");
    let runs: Vec<StoredRun> = other.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_all_stops_running_and_queued() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(1, fake("claude_sleeps.sh")).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 2).await;
    let (t1, t2) = (threads[0], threads[1]);
    c.call(Request::Subscribe { thread: t1 }).await.unwrap();
    c.call(Request::Subscribe { thread: t2 }).await.unwrap();
    for thread in [t1, t2] {
        c.call(Request::PostMessage {
            thread,
            body: "@Forge wait".into(),
        })
        .await
        .unwrap();
    }
    let seen = c
        .until(|m| {
            matches!(
                m,
                ServerMsg::RunStatus {
                    status: RunStatus::Working,
                    ..
                }
            )
        })
        .await;
    let Some(ServerMsg::RunStatus { thread, run, .. }) = seen.last().cloned() else {
        unreachable!()
    };
    assert_eq!(thread, t1);
    let working: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Working),
        })
        .await;
    assert_eq!(working.len(), 1, "cap 1 allows one run");

    assert_eq!(
        c.call(Request::StopAll).await.unwrap(),
        serde_json::Value::Null
    );

    let mut stopped_status: Option<RunId> = None;
    let mut t1_msg = None;
    let mut t2_msg = None;
    while stopped_status.is_none() || t1_msg.is_none() || t2_msg.is_none() {
        match c.recv().await.expect("connection closed") {
            ServerMsg::RunStatus {
                run,
                status: RunStatus::Stopped,
                ..
            } => stopped_status = Some(run),
            ServerMsg::Message { thread, message } if message.author == Author::System => {
                if thread == t1 {
                    t1_msg = Some(message.body);
                } else if thread == t2 {
                    t2_msg = Some(message.body);
                }
            }
            _ => {}
        }
    }
    assert_eq!(stopped_status, Some(run));
    assert_eq!(t1_msg.as_deref(), Some("Forge stopped."));
    assert_eq!(t2_msg.as_deref(), Some("Stopped before starting."));

    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 1, "the queued job never got a run");
    assert_eq!(runs[0].status, RunStatus::Stopped);

    // Nothing starts later from the dropped queue.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let working: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Working),
        })
        .await;
    assert!(working.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_is_0600() {
    let d = Daemon::start(4, fake("claude_sleeps.sh")).await;
    let mode = std::fs::metadata(d.socket()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_in_thread_resumes_agent_session() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = logging_replay_script(dir.path(), "read_file.jsonl");
    let d = Daemon::start(4, script).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();

    c.call(Request::PostMessage {
        thread,
        body: "hello".into(),
    })
    .await
    .unwrap();
    for body in ["@Forge look at it", "@Forge and again"] {
        c.call(Request::PostMessage {
            thread,
            body: body.into(),
        })
        .await
        .unwrap();
        c.until(|m| {
            matches!(
                m,
                ServerMsg::RunStatus {
                    status: RunStatus::Done,
                    ..
                }
            )
        })
        .await;
    }

    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 2);
    let session = arg_after(&calls[0], "--session-id").expect("new session id");
    assert_eq!(
        arg_after(&calls[0], "-p").unwrap(),
        "Alex: hello\n\n@Forge look at it"
    );
    assert_eq!(arg_after(&calls[1], "--resume"), Some(session.clone()));
    assert_eq!(arg_after(&calls[1], "-p").unwrap(), "@Forge and again");

    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.session_id == session));
}
