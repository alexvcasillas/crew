//! The client against a real crewd on a temp home.

#[allow(dead_code)]
#[path = "../src/test_support.rs"]
mod test_support;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crew_app::client::{self, ClientEvent, ClientHandle};
use crew_app::connection::ConnectionConfig;
use crew_app::model::{ConnectionState, State};
use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crew_core::store::Author;
use crew_proto::{Request, ServerMsg};
use futures::channel::mpsc::{TryRecvError, UnboundedReceiver};
use serde::de::DeserializeOwned;
use test_support::{Daemon, short_home};

/// Generous: CI runners are slow, and reconnecting waits out a backoff.
const WAIT: Duration = Duration::from_secs(30);

/// A claude that prints a recorded session reading a file.
fn fake_claude(dir: &Path) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../crew-core/tests/fixtures/claude/read_file.jsonl");
    let bin = dir.join("claude.sh");
    std::fs::write(&bin, format!("#!/bin/sh\ncat '{}'\n", fixture.display())).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
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
}

#[test]
fn client_reconnects_after_daemon_restart_without_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path());
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let home = short_home();
    let daemon = Daemon::start_with(home.path().to_path_buf(), claude.clone());

    let (client, events) = client::spawn(ConnectionConfig {
        crew_home: home.path().to_path_buf(),
    });
    let mut w = Watcher {
        events,
        state: State::default(),
        seen: Vec::new(),
    };
    w.until("the first snapshot", |w| {
        w.connected() && w.snapshots() == 1
    });

    let project: ProjectId = ask(
        &client,
        Request::CreateProject {
            name: "demo".into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
        },
    );
    let channel: ChannelId = ask(
        &client,
        Request::CreateChannel {
            project,
            name: "general".into(),
        },
    );
    let _: crew_core::ids::AgentId = ask(&client, Request::CreateAgent { spec: forge() });
    let thread: ThreadId = ask(
        &client,
        Request::CreateThread {
            channel,
            title: "read the file".into(),
        },
    );
    w.state.watch(thread);
    client.watch_thread(thread);
    let _: MessageId = ask(
        &client,
        Request::PostMessage {
            thread,
            body: "@Forge read a.txt".into(),
        },
    );

    let done = |w: &Watcher| {
        let replied = w
            .state
            .messages(thread)
            .iter()
            .any(|m| matches!(m.author, Author::Agent(_)));
        let ended = w.seen.iter().any(|e| {
            matches!(
                e,
                ClientEvent::Msg(ServerMsg::RunStatus {
                    status: RunStatus::Done,
                    ..
                })
            )
        });
        replied && ended
    };
    w.until("the run to end", done);
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
    w.until("the disconnect", |w| {
        w.state.connection() == Some(&ConnectionState::NotRunning)
    });
    let mark = w.seen.len();

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
    let mut events_sent: HashMap<(RunId, u64), usize> = HashMap::new();
    let mut messages_sent: HashMap<MessageId, usize> = HashMap::new();
    for event in &w.seen[mark..] {
        match event {
            ClientEvent::Msg(ServerMsg::Event { run, seq, .. }) => {
                *events_sent.entry((*run, *seq)).or_default() += 1;
            }
            ClientEvent::Msg(ServerMsg::Message { message, .. }) => {
                *messages_sent.entry(message.id).or_default() += 1;
            }
            _ => {}
        }
    }
    assert_eq!(events_sent.len(), events_before.len());
    assert!(events_sent.values().all(|n| *n == 1), "{events_sent:?}");
    assert_eq!(messages_sent.len(), messages_before.len());
    assert!(messages_sent.values().all(|n| *n == 1), "{messages_sent:?}");
}
