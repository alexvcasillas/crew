//! Requests and broadcasts the app relies on.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::{Author, StoredAgent, StoredThread};
use crew_proto::{
    ClientMsg, PROTO_VERSION, Request, RuntimeInfo, ServerMsg, read_frame, write_frame,
};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

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

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

/// A claude that replays a fixture and logs its arguments to `args.log`,
/// one line per call.
fn logging_replay(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("args.log");
    let bin = script(
        &dir.join("claude-logging.sh"),
        &format!(
            "echo \"$*\" >> '{log}'\nexec '{ok}' '{fixture}'\n",
            log = log.display(),
            ok = fake("claude_ok.sh").display(),
            fixture = fixture("read_file.jsonl").display(),
        ),
    );
    (bin, log)
}

struct Daemon {
    _tmp: TempDir,
    home: PathBuf,
}

impl Daemon {
    async fn start(claude_bin: PathBuf) -> Daemon {
        // Never the real codex.
        Daemon::start_with(claude_bin, PathBuf::from("/nonexistent/codex")).await
    }

    async fn start_with(claude_bin: PathBuf, codex_bin: PathBuf) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        let path = home.clone();
        tokio::spawn(async move {
            let opts = ServeOptions {
                claude_bin: Some(claude_bin),
                codex_bin: Some(codex_bin),
                crew_mcp_bin: None,
                ..Default::default()
            };
            if let Err(e) = serve(&path, opts).await {
                panic!("serve failed: {e:#}");
            }
        });
        let sock = home.join("crewd.sock");
        timeout(WAIT, async {
            while UnixStream::connect(&sock).await.is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("crewd did not start");
        Daemon { _tmp: tmp, home }
    }

    async fn client(&self) -> Client {
        let mut c = Client {
            stream: UnixStream::connect(self.home.join("crewd.sock"))
                .await
                .unwrap(),
            next_id: 1,
            pending: Vec::new(),
        };
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
    pending: Vec<ServerMsg>,
}

impl Client {
    async fn call(&mut self, req: Request) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.stream, &ClientMsg::Request { id, req })
            .await
            .unwrap();
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

    async fn recv(&mut self) -> ServerMsg {
        if !self.pending.is_empty() {
            return self.pending.remove(0);
        }
        timeout(WAIT, read_frame(&mut self.stream))
            .await
            .expect("timed out waiting for crewd")
            .unwrap()
            .expect("connection closed")
    }

    async fn until(&mut self, mut done: impl FnMut(&ServerMsg) -> bool) -> Vec<ServerMsg> {
        let mut seen = Vec::new();
        loop {
            let msg = self.recv().await;
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
        permission_mode: PermissionMode::ReadOnly,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    }
}

/// A project rooted at `root`, one channel and the Forge agent.
async fn setup(c: &mut Client, root: &Path) -> (ProjectId, ChannelId, AgentId) {
    std::fs::create_dir_all(root).unwrap();
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
    (project, channel, agent)
}

async fn thread(c: &mut Client, channel: ChannelId, title: &str) -> ThreadId {
    c.ok(Request::CreateThread {
        channel,
        title: title.into(),
    })
    .await
}

fn status_of(m: &ServerMsg) -> Option<(ThreadId, RunId, RunStatus)> {
    match m {
        ServerMsg::RunStatus {
            thread,
            run,
            status,
            details,
        } => {
            // The full run comes along, as saved.
            assert_eq!((details.id, details.thread), (*run, *thread));
            assert_eq!(&details.status, status);
            assert_eq!(
                details.ended_at.is_some(),
                !matches!(status, RunStatus::Queued | RunStatus::Working)
            );
            Some((*thread, *run, status.clone()))
        }
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribe_all_sees_status_across_threads_without_events() {
    let dir = tempfile::tempdir().unwrap();
    let (bin, _) = logging_replay(dir.path());
    let d = Daemon::start(bin).await;
    let mut c = d.client().await;
    let (project, channel, agent) = setup(&mut c, &dir.path().join("repo")).await;
    let a = thread(&mut c, channel, "first").await;
    let b = thread(&mut c, channel, "second").await;

    assert_eq!(
        c.call(Request::SubscribeAll).await.unwrap(),
        serde_json::Value::Null
    );
    let ServerMsg::Snapshot {
        projects,
        channels,
        threads,
        agents,
        runs,
        approvals,
    } = c.recv().await
    else {
        panic!("expected a snapshot first");
    };
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, project);
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].id, channel);
    let ids: Vec<ThreadId> = threads.iter().map(|t| t.id).collect();
    assert_eq!(ids, vec![a, b]);
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].id, agent);
    assert_eq!(agents[0].deleted_at, None);
    assert!(runs.is_empty());
    assert!(approvals.is_empty());

    // Also following thread `a` must not send its notices twice.
    c.call(Request::Subscribe { thread: a }).await.unwrap();

    for t in [a, b] {
        c.call(Request::PostMessage {
            thread: t,
            body: "@Forge look".into(),
        })
        .await
        .unwrap();
    }
    let mut done = Vec::new();
    let seen = c
        .until(|m| {
            if let Some((t, _, RunStatus::Done)) = status_of(m) {
                done.push(t);
            }
            done.len() == 2
        })
        .await;

    for t in [a, b] {
        let statuses: Vec<RunStatus> = seen
            .iter()
            .filter_map(status_of)
            .filter(|(th, _, _)| *th == t)
            .map(|(_, _, s)| s)
            .collect();
        assert_eq!(
            statuses,
            vec![RunStatus::Queued, RunStatus::Working, RunStatus::Done],
            "statuses of one thread"
        );
        let yours = seen
            .iter()
            .filter(|m| {
                matches!(m, ServerMsg::Message { thread, message }
                    if *thread == t && message.author == Author::You)
            })
            .count();
        assert_eq!(yours, 1, "your message once per thread");
    }
    assert!(
        seen.iter()
            .any(|m| matches!(m, ServerMsg::Event { thread, .. } if *thread == a)),
        "events of the subscribed thread"
    );
    assert!(
        !seen
            .iter()
            .any(|m| matches!(m, ServerMsg::Event { thread, .. } if *thread == b)),
        "no events of a thread followed only through SubscribeAll"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_created_is_broadcast() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake("claude_sleeps.sh")).await;
    let mut app = d.client().await;
    let mut cli = d.client().await;
    let (_, channel, _) = setup(&mut cli, &dir.path().join("repo")).await;

    app.call(Request::SubscribeAll).await.unwrap();
    assert!(matches!(app.recv().await, ServerMsg::Snapshot { .. }));

    let id = thread(&mut cli, channel, "from the cli").await;
    let ServerMsg::ThreadCreated {
        channel: got_channel,
        thread,
    } = app.recv().await
    else {
        panic!("expected ThreadCreated");
    };
    assert_eq!(got_channel, channel);
    assert_eq!(thread.id, id);
    assert_eq!(thread.title, "from the cli");

    cli.call(Request::RenameThread {
        thread: id,
        title: "  renamed  ".into(),
    })
    .await
    .unwrap();
    let ServerMsg::ThreadChanged { thread } = app.recv().await else {
        panic!("expected ThreadChanged");
    };
    assert_eq!((thread.id, thread.title.as_str()), (id, "renamed"));
    let threads: Vec<StoredThread> = cli.ok(Request::ListThreads { channel }).await;
    assert_eq!(threads[0].title, "renamed");
    assert_eq!(
        cli.call(Request::RenameThread {
            thread: id,
            title: " ".into(),
        })
        .await
        .unwrap_err(),
        "Give the thread a title."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn project_channel_and_thread_changes_are_broadcast() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake("claude_sleeps.sh")).await;
    let mut app = d.client().await;
    let mut cli = d.client().await;
    app.call(Request::SubscribeAll).await.unwrap();
    assert!(matches!(app.recv().await, ServerMsg::Snapshot { .. }));

    let project: ProjectId = cli
        .ok(Request::CreateProject {
            name: "demo".into(),
            root: dir.path().to_string_lossy().into_owned(),
            main_branch: "main".into(),
        })
        .await;
    let ServerMsg::ProjectChanged { project: got } = app.recv().await else {
        panic!("expected ProjectChanged");
    };
    assert_eq!((got.id, got.name.as_str()), (project, "demo"));
    cli.call(Request::UpdateProject {
        project,
        setup_command: Some("make".into()),
        copy_files: vec![".env".into()],
    })
    .await
    .unwrap();
    let ServerMsg::ProjectChanged { project: got } = app.recv().await else {
        panic!("expected ProjectChanged");
    };
    assert_eq!(got.setup_command.as_deref(), Some("make"));
    assert_eq!(got.copy_files, vec![".env".to_string()]);

    let channel: ChannelId = cli
        .ok(Request::CreateChannel {
            project,
            name: "general".into(),
        })
        .await;
    let ServerMsg::ChannelChanged { channel: got } = app.recv().await else {
        panic!("expected ChannelChanged");
    };
    assert_eq!(
        (got.id, got.project, got.name.as_str()),
        (channel, project, "general")
    );
    cli.call(Request::SetChannelHandoff {
        channel,
        policy: Some(HandoffPolicy::Budget),
        budget: Some(2),
    })
    .await
    .unwrap();
    let ServerMsg::ChannelChanged { channel: got } = app.recv().await else {
        panic!("expected ChannelChanged");
    };
    assert_eq!(got.handoff_policy, Some(HandoffPolicy::Budget));
    assert_eq!(got.handoff_budget, Some(2));

    let t = thread(&mut cli, channel, "first").await;
    assert!(matches!(app.recv().await, ServerMsg::ThreadCreated { .. }));
    cli.call(Request::SetThreadHandoff {
        thread: t,
        policy: Some(HandoffPolicy::PerAgent),
        budget: None,
    })
    .await
    .unwrap();
    let ServerMsg::ThreadChanged { thread: got } = app.recv().await else {
        panic!("expected ThreadChanged");
    };
    assert_eq!(got.id, t);
    assert_eq!(got.handoff_policy, Some(HandoffPolicy::PerAgent));
}

#[tokio::test(flavor = "multi_thread")]
async fn update_agent_changes_next_run_args() {
    let dir = tempfile::tempdir().unwrap();
    let (bin, log) = logging_replay(dir.path());
    let d = Daemon::start(bin).await;
    let mut app = d.client().await;
    let mut c = d.client().await;
    let (_, channel, agent) = setup(&mut c, &dir.path().join("repo")).await;
    let t = thread(&mut c, channel, "first").await;
    app.call(Request::SubscribeAll).await.unwrap();
    assert!(matches!(app.recv().await, ServerMsg::Snapshot { .. }));

    let spec = AgentSpec {
        model: "claude-opus-4-5".into(),
        ..forge()
    };
    c.call(Request::UpdateAgent { agent, spec }).await.unwrap();
    let ServerMsg::AgentChanged { agent: changed } = app.recv().await else {
        panic!("expected AgentChanged");
    };
    assert_eq!(changed.id, agent);
    assert_eq!(changed.spec.model, "claude-opus-4-5");

    let agents: Vec<StoredAgent> = c
        .ok(Request::ListAgents {
            include_deleted: false,
        })
        .await;
    assert_eq!(agents[0].spec.model, "claude-opus-4-5");

    c.call(Request::PostMessage {
        thread: t,
        body: "@Forge look".into(),
    })
    .await
    .unwrap();
    app.until(|m| matches!(status_of(m), Some((_, _, RunStatus::Done))))
        .await;
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(
        args.contains("--model claude-opus-4-5"),
        "run did not use the new model: {args}"
    );

    // A name another agent has is refused.
    let other: AgentId = c
        .ok(Request::CreateAgent {
            spec: AgentSpec {
                name: "Sentinel".into(),
                ..forge()
            },
        })
        .await;
    let err = c
        .call(Request::UpdateAgent {
            agent: other,
            spec: forge(),
        })
        .await
        .unwrap_err();
    assert_eq!(err, "an agent named \"Forge\" already exists");
    let err = c
        .call(Request::UpdateAgent {
            agent: other,
            spec: AgentSpec {
                runtime: Runtime::Codex,
                ..forge()
            },
        })
        .await
        .unwrap_err();
    assert_eq!(err, "Codex agents aren't supported yet. Pick Claude Code.");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_agent_refused_while_running() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake("claude_sleeps.sh")).await;
    let mut app = d.client().await;
    let mut c = d.client().await;
    let (_, channel, agent) = setup(&mut c, &dir.path().join("repo")).await;
    let t = thread(&mut c, channel, "first").await;
    app.call(Request::SubscribeAll).await.unwrap();
    assert!(matches!(app.recv().await, ServerMsg::Snapshot { .. }));

    c.call(Request::PostMessage {
        thread: t,
        body: "@Forge wait".into(),
    })
    .await
    .unwrap();
    let seen = app
        .until(|m| matches!(status_of(m), Some((_, _, RunStatus::Working))))
        .await;
    let (_, run, _) = seen.iter().find_map(status_of).unwrap();

    let err = c.call(Request::DeleteAgent { agent }).await.unwrap_err();
    assert_eq!(err, "Stop Forge's runs first.");

    c.call(Request::StopRun { run }).await.unwrap();
    app.until(|m| matches!(status_of(m), Some((_, _, RunStatus::Stopped))))
        .await;
    c.call(Request::DeleteAgent { agent }).await.unwrap();
    let seen = app
        .until(|m| matches!(m, ServerMsg::AgentChanged { agent: a } if a.id == agent))
        .await;
    let Some(ServerMsg::AgentChanged { agent: deleted }) = seen.last() else {
        unreachable!()
    };
    assert!(deleted.deleted_at.is_some(), "sent as deleted");

    let agents: Vec<StoredAgent> = c
        .ok(Request::ListAgents {
            include_deleted: false,
        })
        .await;
    assert!(agents.is_empty());
    let agents: Vec<StoredAgent> = c
        .ok(Request::ListAgents {
            include_deleted: true,
        })
        .await;
    assert_eq!(agents.len(), 1);
    assert_eq!(
        (agents[0].id, agents[0].spec.name.as_str()),
        (agent, "Forge")
    );
    assert!(agents[0].deleted_at.is_some());

    // A mention of the deleted agent starts nothing.
    c.call(Request::PostMessage {
        thread: t,
        body: "@Forge again".into(),
    })
    .await
    .unwrap();
    app.until(|m| {
        matches!(m, ServerMsg::Message { message, .. }
            if message.body == "No agent named @Forge.")
    })
    .await;

    // Its history stays, and the snapshot still names it.
    app.call(Request::SubscribeAll).await.unwrap();
    let ServerMsg::Snapshot { agents, runs, .. } = app.recv().await else {
        panic!("expected a snapshot");
    };
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].spec.name, "Forge");
    assert!(agents[0].deleted_at.is_some());
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].agent, agent);

    // The name is free again.
    let again: AgentId = c.ok(Request::CreateAgent { spec: forge() }).await;
    assert_ne!(again, agent);
    assert!(c.call(Request::DeleteAgent { agent }).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn runtimes_reports_fake_claude_version() {
    let dir = tempfile::tempdir().unwrap();
    let bin = script(
        &dir.path().join("claude-fake.sh"),
        "case \"$1 $2\" in\n\
         '--version '*) echo '9.8.7 (Claude Code)' ;;\n\
         'auth status') echo '{\"loggedIn\":true}' ;;\n\
         *) exit 2 ;;\n\
         esac\n",
    );
    let codex = script(
        &dir.path().join("codex-fake.sh"),
        "[ \"$1\" = --version ] && echo 'codex-cli 0.9.1' && exit 0\nexit 1\n",
    );
    let d = Daemon::start_with(bin.clone(), codex.clone()).await;
    let mut c = d.client().await;
    let list: Vec<RuntimeInfo> = c.ok(Request::Runtimes).await;
    let claude = list
        .iter()
        .find(|r| r.runtime == Runtime::ClaudeCode)
        .expect("claude in the list");
    assert!(claude.installed);
    assert_eq!(claude.version.as_deref(), Some("9.8.7"));
    assert_eq!(claude.signed_in, Some(true));
    assert_eq!(claude.path.as_deref(), Some(bin.to_str().unwrap()));
    let codex_info = list
        .iter()
        .find(|r| r.runtime == Runtime::Codex)
        .expect("codex in the list");
    assert!(codex_info.installed);
    assert_eq!(codex_info.version.as_deref(), Some("0.9.1"));
    assert_eq!(codex_info.signed_in, None);
    assert_eq!(codex_info.path.as_deref(), Some(codex.to_str().unwrap()));

    // Signed out: the status command fails and says so.
    let bin = script(
        &dir.path().join("claude-out.sh"),
        "case \"$1 $2\" in\n\
         '--version '*) echo '9.8.7 (Claude Code)' ;;\n\
         'auth status') echo '{\"loggedIn\":false}'; exit 1 ;;\n\
         esac\n",
    );
    let d = Daemon::start(bin).await;
    let mut c = d.client().await;
    let list: Vec<RuntimeInfo> = c.ok(Request::Runtimes).await;
    assert_eq!(list[0].runtime, Runtime::ClaudeCode);
    assert_eq!(list[0].signed_in, Some(false));

    // No status command: unknown.
    let bin = script(
        &dir.path().join("claude-old.sh"),
        "[ \"$1\" = --version ] && echo '1.0.0 (Claude Code)' && exit 0\nexit 1\n",
    );
    let d = Daemon::start(bin).await;
    let mut c = d.client().await;
    let list: Vec<RuntimeInfo> = c.ok(Request::Runtimes).await;
    assert_eq!(list[0].version.as_deref(), Some("1.0.0"));
    assert_eq!(list[0].signed_in, None);

    // Found but its --version fails: still installed, version unknown.
    let bin = script(&dir.path().join("claude-mute.sh"), "exit 3\n");
    let d = Daemon::start(bin.clone()).await;
    let mut c = d.client().await;
    let list: Vec<RuntimeInfo> = c.ok(Request::Runtimes).await;
    assert!(list[0].installed);
    assert_eq!(list[0].version, None);
    assert_eq!(list[0].signed_in, None);
    assert_eq!(list[0].path.as_deref(), Some(bin.to_str().unwrap()));

    // Missing binary.
    let d = Daemon::start(dir.path().join("no-such-claude")).await;
    let mut c = d.client().await;
    let list: Vec<RuntimeInfo> = c.ok(Request::Runtimes).await;
    assert!(!list[0].installed);
    assert_eq!(list[0].version, None);
    assert_eq!(list[0].signed_in, None);
    assert_eq!(list[0].path, None);
    assert!(!list[1].installed, "the default test daemon has no codex");
}

/// A CLI that is slow to say its version doesn't hold the answer back:
/// crewd answers with what it found at once and sends the rest later.
#[tokio::test]
async fn slow_runtimes_answer_early_and_follow_up() {
    let dir = tempfile::tempdir().unwrap();
    let bin = script(
        &dir.path().join("claude-slow.sh"),
        "sleep 2\ncase \"$1 $2\" in\n\
         '--version '*) echo '9.8.7 (Claude Code)' ;;\n\
         'auth status') echo '{\"loggedIn\":true}' ;;\n\
         esac\n",
    );
    let d = Daemon::start_with(bin.clone(), PathBuf::from("/nonexistent/codex")).await;
    let mut app = d.client().await;
    app.call(Request::SubscribeAll).await.unwrap();
    let mut c = d.client().await;
    let started = std::time::Instant::now();
    let early: Vec<RuntimeInfo> = c.ok(Request::Runtimes).await;
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "answered in {:?}",
        started.elapsed()
    );
    let claude = early
        .iter()
        .find(|r| r.runtime == Runtime::ClaudeCode)
        .unwrap();
    assert!(claude.installed);
    assert_eq!(claude.path.as_deref(), Some(bin.to_str().unwrap()));
    assert_eq!(claude.version, None, "not known yet");
    let later = loop {
        if let ServerMsg::RuntimesChanged { runtimes } = app.recv().await {
            break runtimes;
        }
    };
    let claude = later
        .iter()
        .find(|r| r.runtime == Runtime::ClaudeCode)
        .unwrap();
    assert_eq!(claude.version.as_deref(), Some("9.8.7"));
    assert_eq!(claude.signed_in, Some(true));
    let codex = later.iter().find(|r| r.runtime == Runtime::Codex).unwrap();
    assert!(!codex.installed);
}
