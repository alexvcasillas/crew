use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_core::store::{RuntimeUsage, StoredRun};
use crew_proto::{ClientMsg, Limits, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use crewd::{ServeOptions, serve_until};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// A fake claude that answers at once, or one that waits until stopped.
fn fake_claude(dir: &Path, sleeps: bool) -> PathBuf {
    let path = dir.join("claude-fake.sh");
    let body = if sleeps {
        "sleep 60\n"
    } else {
        concat!(
            "printf '{\"type\":\"system\",\"subtype\":\"init\",\"cwd\":\"/work/demo\",\"session_id\":\"s\",\"model\":\"m\",\"tools\":[]}\\n'\n",
            "printf '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"duration_ms\":1,\"num_turns\":1,\"permission_denials\":[],\"session_id\":\"s\"}\\n'\n",
        )
    };
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Daemon {
    stop: Option<oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    home: PathBuf,
}

impl Daemon {
    async fn start(home: &Path, max_runs: usize, claude: PathBuf) -> Daemon {
        let opts = ServeOptions {
            no_title_runs: true,
            max_runs,
            claude_bin: Some(claude),
            crew_mcp_bin: None,
            ..Default::default()
        };
        let (stop, stop_rx) = oneshot::channel::<()>();
        let path = home.to_path_buf();
        let server = tokio::spawn(async move {
            serve_until(&path, opts, async {
                let _ = stop_rx.await;
            })
            .await
        });
        let sock = home.join("crewd.sock");
        timeout(WAIT, async {
            while UnixStream::connect(&sock).await.is_err() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("crewd did not start");
        Daemon {
            stop: Some(stop),
            server,
            home: home.to_path_buf(),
        }
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        timeout(WAIT, self.server)
            .await
            .expect("crewd did not stop")
            .unwrap()
            .unwrap();
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
                .expect("timed out waiting for crewd")
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

    async fn until(&mut self, mut done: impl FnMut(&ServerMsg) -> bool) -> Vec<ServerMsg> {
        let mut seen = Vec::new();
        loop {
            let msg = if self.pending.is_empty() {
                timeout(WAIT, read_frame(&mut self.stream))
                    .await
                    .expect("timed out waiting for crewd")
                    .unwrap()
                    .expect("connection closed")
            } else {
                self.pending.remove(0)
            };
            let stop = done(&msg);
            seen.push(msg);
            if stop {
                return seen;
            }
        }
    }

    async fn runs(&mut self, status: RunStatus) -> Vec<StoredRun> {
        self.ok(Request::ListRuns {
            status: Some(status),
        })
        .await
    }
}

fn is_status(m: &ServerMsg, want: RunStatus) -> bool {
    matches!(m, ServerMsg::RunStatus { status, .. } if *status == want)
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
        max_run_minutes: 0,
    }
}

/// A project, a channel, the Forge agent and `n` threads, all subscribed.
async fn setup(c: &mut Client, root: &Path, n: usize) -> (AgentId, Vec<ThreadId>) {
    let project: ProjectId = c
        .ok(Request::CreateProject {
            name: "demo".into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
            work_mode: None,
            copy_files: vec![],
            setup_command: None,
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
        let thread: ThreadId = c
            .ok(Request::CreateThread {
                channel,
                title: format!("thread {i}"),
            })
            .await;
        c.call(Request::Subscribe { thread }).await.unwrap();
        threads.push(thread);
    }
    (agent, threads)
}

async fn say(c: &mut Client, thread: ThreadId, body: &str) {
    c.call(Request::PostMessage {
        thread,
        body: body.into(),
        skill: None,
        model_override: None,
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_holds_new_runs_until_unpaused() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Daemon::start(&tmp.path().join("crew"), 4, fake_claude(tmp.path(), false)).await;
    let mut c = d.client().await;
    let mut other = d.client().await;
    other.call(Request::SubscribeAll).await.unwrap();
    let (_, threads) = setup(&mut c, tmp.path(), 1).await;
    let thread = threads[0];

    let paused = Limits {
        paused: true,
        ..Limits::default()
    };
    c.call(Request::SetLimits { limits: paused }).await.unwrap();
    let seen = other
        .until(|m| matches!(m, ServerMsg::LimitsChanged { .. }))
        .await;
    assert_eq!(
        seen.last(),
        Some(&ServerMsg::LimitsChanged { limits: paused })
    );

    say(&mut c, thread, "@Forge go").await;
    c.until(|m| is_status(m, RunStatus::Queued)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(c.runs(RunStatus::Queued).await.len(), 1);
    assert!(c.runs(RunStatus::Done).await.is_empty());

    c.call(Request::SetLimits {
        limits: Limits::default(),
    })
    .await
    .unwrap();
    c.until(|m| is_status(m, RunStatus::Done)).await;
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn limits_survive_a_restart_and_beat_the_seed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    let d = Daemon::start(&home, 4, fake_claude(tmp.path(), false)).await;
    let mut c = d.client().await;
    let limits = Limits {
        max_runs: 2,
        paused: true,
        ..Limits::default()
    };
    c.call(Request::SetLimits { limits }).await.unwrap();
    drop(c);
    d.stop().await;

    // The seed is ignored now that a value is saved.
    let d = Daemon::start(&home, 6, fake_claude(tmp.path(), false)).await;
    let mut c = d.client().await;
    c.call(Request::SubscribeAll).await.unwrap();
    let seen = c.until(|m| matches!(m, ServerMsg::Snapshot { .. })).await;
    let Some(ServerMsg::Snapshot { limits: got, .. }) = seen.last() else {
        unreachable!()
    };
    assert_eq!(*got, limits);

    // Still paused after the restart: a new run waits in the queue.
    let (_, threads) = setup(&mut c, tmp.path(), 1).await;
    say(&mut c, threads[0], "@Forge go").await;
    c.until(|m| is_status(m, RunStatus::Queued)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(c.runs(RunStatus::Queued).await.len(), 1);
    assert!(c.runs(RunStatus::Done).await.is_empty());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lowering_the_cap_keeps_live_runs_and_holds_new_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Daemon::start(&tmp.path().join("crew"), 4, fake_claude(tmp.path(), true)).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, tmp.path(), 3).await;
    say(&mut c, threads[0], "@Forge a").await;
    say(&mut c, threads[1], "@Forge b").await;
    for _ in 0..2 {
        c.until(|m| is_status(m, RunStatus::Working)).await;
    }

    c.call(Request::SetLimits {
        limits: Limits {
            max_runs: 1,
            ..Limits::default()
        },
    })
    .await
    .unwrap();
    say(&mut c, threads[2], "@Forge c").await;
    c.until(|m| is_status(m, RunStatus::Queued)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(c.runs(RunStatus::Working).await.len(), 2);
    assert_eq!(c.runs(RunStatus::Queued).await.len(), 1);

    // One of two live runs ends: still not below the cap of 1.
    c.call(Request::StopThread { thread: threads[0] })
        .await
        .unwrap();
    c.until(|m| is_status(m, RunStatus::Stopped)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(c.runs(RunStatus::Queued).await.len(), 1);

    c.call(Request::StopThread { thread: threads[1] })
        .await
        .unwrap();
    c.until(|m| is_status(m, RunStatus::Working)).await;
    assert!(c.runs(RunStatus::Queued).await.is_empty());
    c.call(Request::StopAll).await.unwrap();
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_limits_rejects_values_out_of_range() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Daemon::start(&tmp.path().join("crew"), 4, fake_claude(tmp.path(), false)).await;
    let mut c = d.client().await;
    let bad = |limits: Limits| Request::SetLimits { limits };
    let base = Limits::default();
    for (limits, want) in [
        (
            Limits {
                max_runs: 0,
                ..base
            },
            "Choose between 1 and 8 agents at once.",
        ),
        (
            Limits {
                max_runs: 9,
                ..base
            },
            "Choose between 1 and 8 agents at once.",
        ),
        (
            Limits {
                handoff_budget: 11,
                ..base
            },
            "Choose between 1 and 10 handoffs.",
        ),
        (
            Limits {
                handoff_budget: 0,
                ..base
            },
            "Choose between 1 and 10 handoffs.",
        ),
        (
            Limits {
                handoff_policy: crew_core::policy::HandoffPolicy::PerAgent,
                ..base
            },
            "Choose Ask first or Run with budget.",
        ),
    ] {
        assert_eq!(c.call(bad(limits)).await.unwrap_err(), want);
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn history_and_usage_answer_over_the_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Daemon::start(&tmp.path().join("crew"), 4, fake_claude(tmp.path(), false)).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, tmp.path(), 1).await;
    for i in 0..3 {
        say(&mut c, threads[0], &format!("@Forge go {i}")).await;
        c.until(|m| is_status(m, RunStatus::Done)).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let page: Vec<StoredRun> = c
        .ok(Request::RunHistory {
            before: None,
            limit: 2,
        })
        .await;
    assert_eq!(page.len(), 2);
    assert!(page[0].ended_at >= page[1].ended_at);
    let rest: Vec<StoredRun> = c
        .ok(Request::RunHistory {
            before: page[1].ended_at,
            limit: 2,
        })
        .await;
    // Inclusive: the page's last run comes again, then the older one.
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].id, page[1].id);
    assert!(rest[1].ended_at < page[1].ended_at);

    let usage: Vec<RuntimeUsage> = c.ok(Request::UsageSince { since_ms: 0 }).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].runtime, Runtime::ClaudeCode);
    assert_eq!(usage[0].runs, 3);
    let none: Vec<RuntimeUsage> = c
        .ok(Request::UsageSince {
            since_ms: i64::MAX / 2,
        })
        .await;
    assert!(none.is_empty());
    d.stop().await;
}
