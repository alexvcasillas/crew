//! The client against a real crewd on a temp home.

#[allow(dead_code)]
#[path = "../src/test_support.rs"]
mod test_support;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crew_app::client::{self, ClientEvent, ClientHandle, LOST, NOT_RUNNING, OUT_OF_DATE};
use crew_app::connection::ConnectionConfig;
use crew_app::model::{ConnectionState, State};
use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crew_core::store::{Author, StoredProject};
use crew_proto::{Request, RuntimeInfo, ServerMsg};
use futures::channel::mpsc::{TryRecvError, UnboundedReceiver};
use serde::de::DeserializeOwned;
use serde_json::Value;
use test_support::{Daemon, always_refuse_hello, short_home};

/// Generous: CI runners are slow, and reconnecting waits out a backoff.
const WAIT: Duration = Duration::from_secs(30);

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../crew-core/tests/fixtures/claude/read_file.jsonl")
}

/// A claude that takes `version_secs` to answer `--version` (so the
/// `Runtimes` request is slow), is signed in, and runs `run` as an agent.
fn fake_claude(dir: &Path, version_secs: u32, run: &str) -> PathBuf {
    let bin = dir.join("claude.sh");
    let body = format!(
        "#!/bin/sh\ncase \"$1\" in\n  --version) sleep {version_secs}; echo '9.9.9 (Claude Code)'; exit 0;;\n  auth) exit 0;;\nesac\n{run}\n"
    );
    std::fs::write(&bin, body).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// A run that prints a recorded session reading a file.
fn reads_a_file() -> String {
    format!("cat '{}'", fixture().display())
}

/// A run that says `n` things in a burst, between the recorded session's
/// first and last lines.
fn bursts(n: u32) -> String {
    let say = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"tick"}]},"session_id":"00000000-0000-4000-8000-000000000001"}"#;
    format!(
        "head -n 1 '{f}'\ni=0\nwhile [ $i -lt {n} ]; do echo '{say}'; i=$((i+1)); done\ntail -n 1 '{f}'",
        f = fixture().display()
    )
}

fn forge() -> AgentSpec {
    AgentSpec {
        name: "Forge".into(),
        role: "Builds things".into(),
        runtime: Runtime::ClaudeCode,
        model: "claude-haiku-4-5".into(),
        instructions: String::new(),
        permission_mode: PermissionMode::ReadOnly,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    }
}

fn ask<T: DeserializeOwned>(client: &ClientHandle, req: Request) -> T {
    let answer = futures::executor::block_on(client.request(req)).unwrap();
    serde_json::from_value(answer).unwrap()
}

/// Sends `req` and waits for its answer on a thread of its own.
fn in_background(client: &ClientHandle, req: Request) -> JoinHandle<Result<Value, String>> {
    let answer = client.request(req);
    std::thread::spawn(move || futures::executor::block_on(answer))
}

/// Feeds what the client reports into a model, keeping a copy.
struct Watcher {
    events: UnboundedReceiver<ClientEvent>,
    state: State,
    seen: Vec<ClientEvent>,
}

impl Watcher {
    /// Takes in events until `done` holds. Panics after [`WAIT`].
    fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let started = Instant::now();
        while !done(self) {
            match self.events.try_recv() {
                Ok(event) => {
                    event.clone().apply_to(&mut self.state);
                    self.seen.push(event);
                }
                Err(TryRecvError::Closed) => panic!("the client stopped"),
                Err(TryRecvError::Empty) => {
                    assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// Takes in whatever arrives during `time`.
    fn settle(&mut self, time: Duration) {
        let until = Instant::now() + time;
        self.until("nothing", |_| Instant::now() >= until);
    }

    fn snapshots(&self) -> usize {
        self.seen
            .iter()
            .filter(|e| matches!(e, ClientEvent::Msg(ServerMsg::Snapshot { .. })))
            .count()
    }

    fn connected(&self) -> bool {
        self.state.connection() == Some(&ConnectionState::Connected)
    }

    /// Whether `thread` has Forge's reply and its run ended.
    fn replied(&self, thread: ThreadId) -> bool {
        let replied = self
            .state
            .messages(thread)
            .iter()
            .any(|m| matches!(m.author, Author::Agent(_)));
        let ended = self.seen.iter().any(|e| {
            matches!(
                e,
                ClientEvent::Msg(ServerMsg::RunStatus {
                    status: RunStatus::Done,
                    ..
                })
            )
        });
        replied && ended
    }
}

/// A client on `home`, after its first snapshot.
fn connect(home: &Path) -> (ClientHandle, Watcher) {
    let (client, events) = client::spawn(ConnectionConfig {
        crew_home: home.to_path_buf(),
    });
    let mut w = Watcher {
        events,
        state: State::default(),
        seen: Vec::new(),
    };
    w.until("the first snapshot", |w| {
        w.connected() && w.snapshots() == 1
    });
    (client, w)
}

/// A project rooted at `root` with one channel, the Forge agent and one
/// thread.
fn setup(client: &ClientHandle, root: &Path) -> ThreadId {
    std::fs::create_dir_all(root).unwrap();
    let project: ProjectId = ask(
        client,
        Request::CreateProject {
            name: "demo".into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
            work_mode: None,
            copy_files: vec![],
            setup_command: None,
        },
    );
    let channel: ChannelId = ask(
        client,
        Request::CreateChannel {
            project,
            name: "general".into(),
        },
    );
    let _: AgentId = ask(client, Request::CreateAgent { spec: forge() });
    ask(
        client,
        Request::CreateThread {
            channel,
            title: "read the file".into(),
        },
    )
}

fn post(client: &ClientHandle, thread: ThreadId) {
    let _: MessageId = ask(
        client,
        Request::PostMessage {
            thread,
            body: "@Forge read a.txt".into(),
            skill: None,
            model_override: None,
        },
    );
}

/// How many times crewd sent each event and message after `seen[mark]`.
fn sent_after(
    w: &Watcher,
    mark: usize,
) -> (HashMap<(RunId, u64), usize>, HashMap<MessageId, usize>) {
    let mut events: HashMap<(RunId, u64), usize> = HashMap::new();
    let mut messages: HashMap<MessageId, usize> = HashMap::new();
    for event in &w.seen[mark..] {
        match event {
            ClientEvent::Msg(ServerMsg::Event { run, seq, .. }) => {
                *events.entry((*run, *seq)).or_default() += 1;
            }
            ClientEvent::Msg(ServerMsg::Message { message, .. }) => {
                *messages.entry(message.id).or_default() += 1;
            }
            _ => {}
        }
    }
    (events, messages)
}

fn runtimes(answer: Result<Value, String>) -> Vec<RuntimeInfo> {
    serde_json::from_value(answer.unwrap()).unwrap()
}

/// Long enough that `Runtimes` waits for a slow fake claude, as these
/// tests use it as a request that takes a while.
const SLOW_RUNTIMES: Duration = Duration::from_secs(10);

#[test]
fn client_reconnects_after_daemon_restart_without_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), 0, &reads_a_file());
    let home = short_home();
    let daemon = Daemon::start_with(home.path().to_path_buf(), claude.clone());
    let (client, mut w) = connect(home.path());
    let thread = setup(&client, &tmp.path().join("repo"));
    w.state.watch(thread);
    client.watch_thread(thread);
    post(&client, thread);

    w.until("the run to end", |w| w.replied(thread));
    w.settle(Duration::from_millis(300));
    assert!(w.state.thread(thread).is_some(), "the new thread arrived");
    let events_before = w.state.events(thread).to_vec();
    let messages_before = w.state.messages(thread).to_vec();
    assert!(
        events_before.len() >= 3,
        "the run's events: {events_before:?}"
    );
    assert_eq!(messages_before.len(), 2, "yours and Forge's reply");

    drop(daemon);
    w.until("crewd to be gone", |w| {
        w.state.connection() == Some(&ConnectionState::NotRunning)
    });
    let mark = w.seen.len();
    let lost = w
        .seen
        .iter()
        .position(|e| matches!(e, ClientEvent::Reconnecting(_)));
    let gone = w
        .seen
        .iter()
        .position(|e| matches!(e, ClientEvent::NotRunning(_)));
    assert!(
        lost < gone,
        "reconnecting first, then not running: {lost:?} {gone:?}"
    );

    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    w.until("the reconnect", |w| {
        w.connected()
            && w.snapshots() == 2
            && w.state.events(thread).len() >= events_before.len()
            && w.state.messages(thread).len() >= messages_before.len()
    });
    w.settle(Duration::from_millis(500));

    assert_eq!(w.state.events(thread), events_before.as_slice());
    assert_eq!(w.state.messages(thread), messages_before.as_slice());
    assert!(w.state.thread(thread).is_some());

    // crewd itself sent each event and message once: the thread was
    // subscribed once on the new connection.
    let (events_sent, messages_sent) = sent_after(&w, mark);
    assert_eq!(events_sent.len(), events_before.len());
    assert!(events_sent.values().all(|n| *n == 1), "{events_sent:?}");
    assert_eq!(messages_sent.len(), messages_before.len());
    assert!(messages_sent.values().all(|n| *n == 1), "{messages_sent:?}");
}

#[test]
fn requests_answered_out_of_order_reach_their_callers() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), 2, &reads_a_file());
    let home = short_home();
    let _daemon = Daemon::start_with_options(
        home.path().to_path_buf(),
        crewd::ServeOptions {
            claude_bin: Some(claude),
            runtimes_wait: Some(SLOW_RUNTIMES),
            ..Default::default()
        },
    );
    let (client, _w) = connect(home.path());

    // Runtimes waits for claude's slow `--version`; crewd answers later
    // requests meanwhile.
    let slow = in_background(&client, Request::Runtimes);
    std::thread::sleep(Duration::from_millis(200));
    let projects: Vec<StoredProject> = ask(&client, Request::ListProjects);
    assert!(projects.is_empty());
    assert!(!slow.is_finished(), "the fast request was answered first");

    let runtimes = runtimes(slow.join().unwrap());
    assert_eq!(runtimes[0].runtime, Runtime::ClaudeCode);
    assert_eq!(runtimes[0].version.as_deref(), Some("9.9.9"));
}

#[test]
fn a_resync_reopens_only_the_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), 4, &bursts(3000));
    let home = short_home();
    // A one-notice buffer: the burst leaves the stream behind.
    let _daemon = Daemon::start_with_options(
        home.path().to_path_buf(),
        crewd::ServeOptions {
            claude_bin: Some(claude),
            notice_buffer: 1,
            runtimes_wait: Some(SLOW_RUNTIMES),
            ..Default::default()
        },
    );
    let (client, mut w) = connect(home.path());
    let thread = setup(&client, &tmp.path().join("repo"));
    w.state.watch(thread);
    client.watch_thread(thread);

    let slow = in_background(&client, Request::Runtimes);
    post(&client, thread);
    w.until("the stream to be reopened", |w| w.snapshots() >= 2);
    assert!(!slow.is_finished(), "the request was in flight");
    w.until("the run to end", |w| w.replied(thread));

    // The control connection stayed: the request got crewd's answer.
    let runtimes = runtimes(slow.join().unwrap());
    assert_eq!(runtimes[0].version.as_deref(), Some("9.9.9"));
    assert!(
        !w.seen
            .iter()
            .any(|e| matches!(e, ClientEvent::Reconnecting(_) | ClientEvent::NotRunning(_))),
        "the app stayed connected"
    );
    assert!(w.connected());
    let _: Vec<StoredProject> = ask(&client, Request::ListProjects);
}

#[test]
fn a_request_in_flight_when_crewd_stops_is_lost_and_later_ones_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), 3, &reads_a_file());
    let home = short_home();
    let daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let (client, mut w) = connect(home.path());

    let slow = in_background(&client, Request::Runtimes);
    std::thread::sleep(Duration::from_millis(200));
    drop(daemon);
    assert_eq!(slow.join().unwrap(), Err(LOST.to_string()));

    w.until("crewd to be gone", |w| {
        w.state.connection() == Some(&ConnectionState::NotRunning)
    });
    let answer = futures::executor::block_on(client.request(Request::ListProjects));
    assert_eq!(answer, Err(NOT_RUNNING.to_string()));
}

#[test]
fn unwatching_then_watching_again_rebuilds_the_log() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), 0, &reads_a_file());
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let (client, mut w) = connect(home.path());
    let thread = setup(&client, &tmp.path().join("repo"));
    w.state.watch(thread);
    client.watch_thread(thread);
    post(&client, thread);
    w.until("the run to end", |w| w.replied(thread));
    w.settle(Duration::from_millis(300));
    let events_before = w.state.events(thread).to_vec();
    assert!(events_before.len() >= 3);

    w.state.unwatch(thread);
    client.unwatch_thread(thread);
    assert!(w.state.events(thread).is_empty());
    let mark = w.seen.len();
    w.state.watch(thread);
    client.watch_thread(thread);
    w.until("the backlog", |w| {
        w.state.events(thread).len() >= events_before.len()
    });
    w.settle(Duration::from_millis(300));

    assert_eq!(w.state.events(thread), events_before.as_slice());
    let (events_sent, _) = sent_after(&w, mark);
    assert_eq!(events_sent.len(), events_before.len());
    assert!(events_sent.values().all(|n| *n == 1), "{events_sent:?}");
}

/// crewd reading a synthetic claude home at `claude_home`. Never the
/// real one.
fn terminal_daemon(home: &Path, claude_home: &Path) -> Daemon {
    Daemon::start_with_options(
        home.to_path_buf(),
        crewd::ServeOptions {
            claude_bin: Some(PathBuf::from("/nonexistent/claude")),
            claude_home: Some(claude_home.to_path_buf()),
            ..Default::default()
        },
    )
}

#[test]
fn a_watched_terminal_session_is_subscribed_again_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let (root, claude_home) = (base.join("repo"), base.join("claude"));
    let folder: String = root
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = claude_home.join("projects").join(folder);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(claude_home.join("sessions")).unwrap();
    let line = |kind: &str, content: Value| {
        serde_json::json!({
            "type": kind,
            "message": {"role": kind, "content": content},
            "sessionId": "s1",
            "timestamp": "2026-11-20T10:00:01.000Z",
            "cwd": root,
            "entrypoint": "cli",
        })
        .to_string()
    };
    let lines = [
        line("user", Value::from("Fix the test")),
        line(
            "assistant",
            serde_json::json!([{"type": "text", "text": "On it."}]),
        ),
    ];
    std::fs::write(dir.join("s1.jsonl"), lines.join("\n") + "\n").unwrap();

    let home = short_home();
    let daemon = terminal_daemon(home.path(), &claude_home);
    let (client, mut w) = connect(home.path());
    setup(&client, &root);
    w.until("the session", |w| w.state.terminal_session("s1").is_some());
    w.state.watch_terminal("s1");
    client.watch_terminal("s1".into());
    w.until("its items", |w| w.state.terminal_items("s1").count() == 2);

    drop(daemon);
    w.until("crewd to be gone", |w| {
        w.state.connection() == Some(&ConnectionState::NotRunning)
    });
    assert_eq!(w.state.terminal_items("s1").count(), 2, "kept meanwhile");
    let _daemon = terminal_daemon(home.path(), &claude_home);
    w.until("the reconnect", |w| w.connected() && w.snapshots() == 2);
    w.until("its items again", |w| {
        w.state.terminal_items("s1").count() == 2
    });

    // Unwatched, nothing more comes for it.
    w.state.unwatch_terminal("s1");
    client.unwatch_terminal("s1".into());
    w.settle(Duration::from_millis(300));
    assert_eq!(w.state.terminal_items("s1").count(), 0);
}

#[test]
fn requests_to_an_out_of_date_crewd_say_so() {
    let home = short_home();
    always_refuse_hello(&home.path().join("crewd.sock"), 2);
    let (client, events) = client::spawn(ConnectionConfig {
        crew_home: home.path().to_path_buf(),
    });
    let mut w = Watcher {
        events,
        state: State::default(),
        seen: Vec::new(),
    };
    w.until("the refusal", |w| {
        matches!(
            w.state.connection(),
            Some(ConnectionState::VersionMismatch { theirs: 2, .. })
        )
    });
    let answer = futures::executor::block_on(client.request(Request::ListProjects));
    assert_eq!(answer, Err(OUT_OF_DATE.to_string()));
}
