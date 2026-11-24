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

/// The prompt: the one argument after `--`, which must come last.
fn prompt_of(args: &[String]) -> String {
    let n = args.len();
    assert!(
        n >= 2 && args[n - 2] == "--",
        "prompt is not last: {args:?}"
    );
    args[n - 1].clone()
}

struct Daemon {
    _tmp: TempDir,
    /// The crew home. It starts out as mode 0755 so tests see crewd fix it.
    home: PathBuf,
}

impl Daemon {
    async fn start(max_runs: usize, claude_bin: PathBuf) -> Daemon {
        Daemon::start_with(max_runs, claude_bin, None).await
    }

    async fn start_with(
        max_runs: usize,
        claude_bin: PathBuf,
        crew_mcp_bin: Option<PathBuf>,
    ) -> Daemon {
        Daemon::start_opts(ServeOptions {
            max_runs,
            claude_bin: Some(claude_bin),
            crew_mcp_bin,
            ..Default::default()
        })
        .await
    }

    async fn start_opts(opts: ServeOptions) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = home.clone();
        tokio::spawn(async move {
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

    fn socket(&self) -> PathBuf {
        self.home.join("crewd.sock")
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
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    }
}

/// Makes `dir` a git repo on `main` with one empty commit, so writing
/// agents can get a worktree there.
fn git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ][..],
    ] {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }
}

/// Makes `root` a git repo, then creates a project rooted there, a channel,
/// the Forge agent and `n` threads.
async fn setup(c: &mut Client, root: &Path, n: usize) -> (AgentId, Vec<ThreadId>) {
    git_repo(root);
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
        "crewd speaks protocol 10, client speaks 99. Restart crewd."
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
async fn usage_gets_the_window_remembered_for_its_model() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(4, replay_script(dir.path(), "usage.jsonl")).await;
    let mut c = d.client().await;
    let (_agent, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();

    let mut usages = Vec::new();
    for body in ["@Forge first", "@Forge second"] {
        let _id: crew_core::ids::MessageId = c
            .ok(Request::PostMessage {
                thread,
                body: body.into(),
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
        usages.push(
            seen.iter()
                .filter_map(|m| match m {
                    ServerMsg::Event {
                        event:
                            Event::Usage {
                                context_tokens,
                                context_window,
                            },
                        ..
                    } => Some((*context_tokens, *context_window)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
    }
    // The first run learns the window from its result line only.
    assert_eq!(
        usages[0],
        vec![(40_807, None), (40_825, None), (40_825, Some(1_000_000))]
    );
    // The second run has it from the start.
    assert_eq!(
        usages[1],
        vec![
            (40_807, Some(1_000_000)),
            (40_825, Some(1_000_000)),
            (40_825, Some(1_000_000))
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_cli_gets_the_run_id() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("run-id.txt");
    let script = dir.path().join("claude-env.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$CREW_RUN_ID\" > '{}'\nexec '{}' '{}'\n",
            out.display(),
            fake("claude_ok.sh").display(),
            fixture("read_file.jsonl").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let d = Daemon::start(4, script).await;
    let mut c = d.client().await;
    let (_agent, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    let _: serde_json::Value = c.ok(Request::Subscribe { thread }).await;
    let _: crew_core::ids::MessageId = c
        .ok(Request::PostMessage {
            thread,
            body: "@Forge go".into(),
        })
        .await;
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

    let runs: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Done),
        })
        .await;
    assert_eq!(runs.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        runs[0].id.to_string()
    );
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
    assert_eq!(
        bodies,
        vec![
            "@Forge fix it",
            "Permission prompts are off: crew-mcp wasn't found next to crewd.",
            "Fixed."
        ]
    );
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

    let mut stopped: Vec<RunId> = Vec::new();
    let mut t1_msg = None;
    let mut t2_msg = None;
    while stopped.len() < 2 || t1_msg.is_none() || t2_msg.is_none() {
        match c.recv().await.expect("connection closed") {
            ServerMsg::RunStatus {
                run,
                status: RunStatus::Stopped,
                ..
            } => stopped.push(run),
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
    assert!(stopped.contains(&run), "the live run was not stopped");
    assert_eq!(t1_msg.as_deref(), Some("Forge stopped."));
    assert_eq!(t2_msg.as_deref(), Some("Stopped before starting."));

    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 2, "the queued job has a run row too");
    assert!(runs.iter().all(|r| r.status == RunStatus::Stopped));
    let queued = runs.iter().find(|r| r.thread == t2).unwrap();
    assert_eq!(queued.session_id, "", "the queued run never started");

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
    let home_mode = std::fs::metadata(&d.home).unwrap().permissions().mode();
    assert_eq!(home_mode & 0o777, 0o700);
}

#[tokio::test(flavor = "multi_thread")]
async fn second_daemon_refuses_and_leaves_runs_alone() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(4, fake("claude_sleeps.sh")).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge wait".into(),
    })
    .await
    .unwrap();
    c.until(|m| {
        matches!(
            m,
            ServerMsg::RunStatus {
                status: RunStatus::Working,
                ..
            }
        )
    })
    .await;

    let err = timeout(WAIT, serve(&d.home, ServeOptions::default()))
        .await
        .expect("second serve should return")
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("already running"),
        "unexpected error: {err:#}"
    );

    let working: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Working),
        })
        .await;
    assert_eq!(working.len(), 1, "the live run must stay Working");
    // The first daemon still answers.
    let mut again = d.client().await;
    let _: Vec<StoredRun> = again.ok(Request::ListRuns { status: None }).await;
    c.call(Request::StopAll).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_spawn_does_not_get_resumed() {
    let dir = tempfile::tempdir().unwrap();
    // Missing for the first run, written before the second.
    let bin = dir.path().join("claude-late.sh");
    let d = Daemon::start(4, bin.clone()).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();

    c.call(Request::PostMessage {
        thread,
        body: "@Forge first".into(),
    })
    .await
    .unwrap();
    c.until(|m| {
        matches!(
            m,
            ServerMsg::RunStatus {
                status: RunStatus::Failed,
                ..
            }
        )
    })
    .await;

    let (script, log) = logging_replay_script(dir.path(), "read_file.jsonl");
    std::fs::rename(&script, &bin).unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge second".into(),
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

    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 1);
    assert!(arg_after(&calls[0], "--session-id").is_some());
    assert!(arg_after(&calls[0], "--resume").is_none());
    let prompt = prompt_of(&calls[0]);
    assert!(
        prompt.starts_with("You: @Forge first\n"),
        "prompt: {prompt}"
    );
    assert!(prompt.ends_with("\n\n@Forge second"), "prompt: {prompt}");
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
    assert_eq!(prompt_of(&calls[0]), "You: hello\n\n@Forge look at it");
    assert_eq!(arg_after(&calls[1], "--resume"), Some(session.clone()));
    assert_eq!(prompt_of(&calls[1]), "@Forge and again");

    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.session_id == session));
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_get_crew_mcp_when_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = logging_replay_script(dir.path(), "read_file.jsonl");
    let mcp_bin = dir.path().join("crew-mcp");
    std::fs::write(&mcp_bin, "").unwrap();
    let d = Daemon::start_with(4, script, Some(mcp_bin.clone())).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, &dir.path().join("repo"), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge look".into(),
    })
    .await
    .unwrap();
    let seen = c.until(|m| is_status(m, RunStatus::Done)).await;

    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 1);
    let args = &calls[0];
    let config: serde_json::Value =
        serde_json::from_str(&arg_after(args, "--mcp-config").expect("--mcp-config")).unwrap();
    let crew = &config["mcpServers"]["crew_run"];
    assert_eq!(crew["type"], "stdio");
    assert_eq!(
        crew["command"],
        std::fs::canonicalize(&mcp_bin).unwrap().to_str().unwrap()
    );
    assert_eq!(
        crew["args"],
        serde_json::json!(["--run", runs[0].id.to_string()])
    );
    assert_eq!(
        crew["env"]["CREW_HOME"],
        std::fs::canonicalize(&d.home).unwrap().to_str().unwrap()
    );
    assert_eq!(
        arg_after(args, "--permission-prompt-tool").as_deref(),
        Some("mcp__crew_run__approve")
    );
    assert_eq!(
        arg_after(args, "--permission-mode").as_deref(),
        Some("default")
    );
    assert!(!args.iter().any(|a| a == "--permission-prompts"));
    let allowed = arg_after(args, "--allowedTools").expect("--allowedTools");
    assert!(
        allowed.split(',').any(|t| t == "mcp__crew_run__handoff"),
        "{allowed}"
    );
    // Edits go through without asking only inside the working folder.
    assert!(allowed.split(',').any(|t| t == "Edit(./**)"), "{allowed}");
    assert!(!allowed.split(',').any(|t| t == "Edit"), "{allowed}");
    // A wired run hears nothing about prompts being off.
    assert!(!seen.iter().any(|m| matches!(
        m,
        ServerMsg::Message { message, .. } if message.author == Author::System
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_hear_their_crew_context_in_one_system_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = logging_replay_script(dir.path(), "read_file.jsonl");
    let mcp_bin = dir.path().join("crew-mcp");
    std::fs::write(&mcp_bin, "").unwrap();
    let d = Daemon::start_with(4, script, Some(mcp_bin)).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, &dir.path().join("repo"), 1).await;
    let scout = AgentSpec {
        name: "Scout".into(),
        role: "Reviews code".into(),
        instructions: "Be brief.".into(),
        permission_mode: PermissionMode::ReadOnly,
        ..forge()
    };
    let _: AgentId = c.ok(Request::CreateAgent { spec: scout }).await;
    let gone: AgentId = c
        .ok(Request::CreateAgent {
            spec: AgentSpec {
                name: "Gone".into(),
                ..forge()
            },
        })
        .await;
    c.call(Request::DeleteAgent { agent: gone }).await.unwrap();
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Scout look".into(),
    })
    .await
    .unwrap();
    c.until(|m| is_status(m, RunStatus::Done)).await;

    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 1);
    let prompts: Vec<&[String]> = calls[0]
        .windows(2)
        .filter(|w| w[0] == "--append-system-prompt")
        .collect();
    assert_eq!(prompts.len(), 1, "{:?}", calls[0]);
    assert_eq!(
        prompts[0][1],
        "You are Scout, Reviews code, working in Crew with other agents in project demo.\n\
         You are read-only: review and report, don't change files.\n\
         When you review changes, list each finding on its own line as `path:line: comment`.\n\
         Other agents here:\n\
         - Forge (Builds things, edits files)\n\
         To bring in another agent, call the mcp__crew_run__handoff tool with the agent's name and a short brief. Writing @Name in your final reply also works.\n\
         \n\
         Be brief."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_runs_get_handoff_but_no_prompt_tool() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = logging_replay_script(dir.path(), "read_file.jsonl");
    let mcp_bin = dir.path().join("crew-mcp");
    std::fs::write(&mcp_bin, "").unwrap();
    let d = Daemon::start_with(4, script, Some(mcp_bin)).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, &dir.path().join("repo"), 1).await;
    let scout = AgentSpec {
        name: "Scout".into(),
        permission_mode: PermissionMode::ReadOnly,
        ..forge()
    };
    let _: AgentId = c.ok(Request::CreateAgent { spec: scout }).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Scout look".into(),
    })
    .await
    .unwrap();
    c.until(|m| is_status(m, RunStatus::Done)).await;

    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 1);
    let args = &calls[0];
    assert!(arg_after(args, "--mcp-config").is_some());
    assert_eq!(
        arg_after(args, "--allowedTools").as_deref(),
        Some("mcp__crew_run__handoff")
    );
    assert_eq!(
        arg_after(args, "--permission-mode").as_deref(),
        Some("plan")
    );
    assert_eq!(arg_after(args, "--permission-prompt-tool"), None);
    assert!(!args.iter().any(|a| a == "--permission-prompts"));
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_go_without_crew_mcp_when_it_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = logging_replay_script(dir.path(), "read_file.jsonl");
    let d = Daemon::start_with(4, script, Some(dir.path().join("no-such-crew-mcp"))).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, &dir.path().join("repo"), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge look".into(),
    })
    .await
    .unwrap();
    let seen = c.until(|m| is_status(m, RunStatus::Done)).await;
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    let notes: Vec<&StoredMessage> = seen
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Message { message, .. } if message.author == Author::System => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(
        notes[0].body,
        "Permission prompts are off: crew-mcp wasn't found next to crewd."
    );
    assert_eq!(notes[0].run, Some(runs[0].id));

    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 1);
    assert_eq!(arg_after(&calls[0], "--mcp-config"), None);
    assert_eq!(
        arg_after(&calls[0], "--permission-prompts").as_deref(),
        Some("none")
    );
}

fn is_status(m: &ServerMsg, want: RunStatus) -> bool {
    matches!(m, ServerMsg::RunStatus { status, .. } if *status == want)
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_run_is_listed_and_can_be_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(1, fake("claude_sleeps.sh")).await;
    let mut c = d.client().await;
    let (agent, threads) = setup(&mut c, dir.path(), 2).await;
    let (t1, t2) = (threads[0], threads[1]);
    c.call(Request::Subscribe { thread: t1 }).await.unwrap();
    c.call(Request::Subscribe { thread: t2 }).await.unwrap();
    c.call(Request::PostMessage {
        thread: t1,
        body: "@Forge wait".into(),
    })
    .await
    .unwrap();
    c.until(|m| is_status(m, RunStatus::Working)).await;

    c.call(Request::PostMessage {
        thread: t2,
        body: "@Forge later".into(),
    })
    .await
    .unwrap();
    let seen = c.until(|m| is_status(m, RunStatus::Queued)).await;
    let Some(ServerMsg::RunStatus { thread, run, .. }) = seen.last().cloned() else {
        unreachable!()
    };
    assert_eq!(thread, t2);

    let queued: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Queued),
        })
        .await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].id, run);
    assert_eq!(queued[0].thread, t2);
    assert_eq!(queued[0].agent, agent);
    assert_eq!(queued[0].session_id, "");

    assert_eq!(
        c.call(Request::StopRun { run }).await.unwrap(),
        serde_json::Value::Null
    );
    let seen = c.until(|m| is_status(m, RunStatus::Stopped)).await;
    assert!(matches!(seen.last(), Some(ServerMsg::RunStatus { run: r, .. }) if *r == run));

    // Free the slot: the stopped run must not start.
    c.call(Request::StopThread { thread: t1 }).await.unwrap();
    c.until(|m| is_status(m, RunStatus::Stopped)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 2);
    let stopped = runs.iter().find(|r| r.id == run).unwrap();
    assert_eq!(stopped.status, RunStatus::Stopped);
    assert_eq!(stopped.session_id, "", "the stopped run never started");
    assert!(stopped.ended_at.is_some());
    assert!(runs.iter().all(|r| r.status == RunStatus::Stopped));
}

#[tokio::test(flavor = "multi_thread")]
async fn resumed_agent_hears_what_it_missed() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = logging_replay_script(dir.path(), "read_file.jsonl");
    let d = Daemon::start(4, script).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let mut triage = forge();
    triage.name = "Triage".into();
    let _: AgentId = c.ok(Request::CreateAgent { spec: triage }).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();

    for body in ["@Forge look at it", "@Triage check it", "@Forge and again"] {
        c.call(Request::PostMessage {
            thread,
            body: body.into(),
        })
        .await
        .unwrap();
        c.until(|m| is_status(m, RunStatus::Done)).await;
    }

    let summary = fixture_summary("read_file.jsonl");
    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 3);
    assert_eq!(prompt_of(&calls[0]), "@Forge look at it");
    assert_eq!(
        prompt_of(&calls[1]),
        format!("You: @Forge look at it\nForge: {summary}\n\n@Triage check it")
    );
    let session = arg_after(&calls[0], "--session-id").unwrap();
    assert_eq!(arg_after(&calls[2], "--resume"), Some(session));
    assert_eq!(
        prompt_of(&calls[2]),
        format!("You: @Triage check it\nTriage: {summary}\n\n@Forge and again")
    );
}

/// A process an agent run started connects to crewd. It may hand off and
/// ask for permission, as crew-mcp does, but nothing else: an agent must
/// not be able to approve its own asks or steer Crew.
#[tokio::test(flavor = "multi_thread")]
async fn connections_from_inside_a_run_may_only_hand_off_and_ask() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let mcp_bin = base.join("crew-mcp");
    std::fs::write(&mcp_bin, "").unwrap();

    // The requests, as crewd reads them. The helper puts the real run id
    // in place of this one.
    let placeholder = RunId::new();
    let other = RunId::new();
    let requests = [
        Request::Hello {
            version: PROTO_VERSION,
        },
        Request::Decide {
            approval: crew_core::ids::ApprovalId::new(),
            approve: true,
            brief: None,
        },
        Request::StopAll,
        Request::CreateAgent { spec: forge() },
        Request::SubscribeAll,
        // Asking as another run.
        Request::Handoff {
            run: other,
            agent: "Nobody".into(),
            brief: "help".into(),
        },
        Request::PermissionAsk {
            run: other,
            tool: "Bash".into(),
            input: serde_json::json!({"command": "make"}),
            tool_use_id: None,
        },
        Request::Handoff {
            run: placeholder,
            agent: "Nobody".into(),
            brief: "help".into(),
        },
        Request::PermissionAsk {
            run: placeholder,
            tool: "Bash".into(),
            input: serde_json::json!({"command": "make"}),
            tool_use_id: Some("toolu_1".into()),
        },
    ];
    let frames = base.join("frames.jsonl");
    let lines: Vec<String> = requests
        .into_iter()
        .enumerate()
        .map(|(i, req)| {
            serde_json::to_string(&ClientMsg::Request {
                id: i as u64 + 1,
                req,
            })
            .unwrap()
        })
        .collect();
    std::fs::write(&frames, lines.join("\n") + "\n").unwrap();
    let answers = base.join("answers.jsonl");
    // Connects as a child of the run, like crew-mcp or a command the
    // agent runs, sends each request and writes down each answer.
    let helper = base.join("agent_client.py");
    std::fs::write(
        &helper,
        format!(
            r#"import json, os, socket, struct, sys
args = sys.argv[1:]
config = json.loads(args[args.index("--mcp-config") + 1])
server = config["mcpServers"]["crew_run"]
run = server["args"][server["args"].index("--run") + 1]
sock = socket.socket(socket.AF_UNIX)
sock.connect(os.path.join(server["env"]["CREW_HOME"], "crewd.sock"))
def read_exact(n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise SystemExit("crewd hung up")
        data += chunk
    return data
out = open({answers:?}, "w")
for line in open({frames:?}):
    body = line.strip().replace({placeholder:?}, run).encode()
    sock.sendall(struct.pack(">I", len(body)) + body)
    while True:
        (size,) = struct.unpack(">I", read_exact(4))
        msg = json.loads(read_exact(size))
        if msg.get("type") == "response":
            out.write(json.dumps(msg) + "\n")
            out.flush()
            break
"#,
            answers = answers.display().to_string(),
            frames = frames.display().to_string(),
            placeholder = placeholder.to_string(),
        ),
    )
    .unwrap();
    let script = base.join("claude-agent-client.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\npython3 '{}' \"$@\" >&2 || exit 9\nexec '{}' '{}'\n",
            helper.display(),
            fake("claude_ok.sh").display(),
            fixture("read_file.jsonl").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let d = Daemon::start_with(4, script, Some(mcp_bin)).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, &base.join("repo"), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge build".into(),
    })
    .await
    .unwrap();
    // The ask from inside the run reaches you, and you decide it.
    let seen = c
        .until(|m| {
            matches!(m, ServerMsg::Approval { approval, .. }
            if approval.status == crew_core::store::ApprovalStatus::Pending)
        })
        .await;
    let Some(ServerMsg::Approval { approval, .. }) = seen.last() else {
        unreachable!()
    };
    c.call(Request::Decide {
        approval: approval.id,
        approve: true,
        brief: None,
    })
    .await
    .unwrap();
    let seen = c
        .until(|m| {
            matches!(m, ServerMsg::RunStatus { status, .. }
                if !matches!(status, RunStatus::Queued | RunStatus::Working))
        })
        .await;
    assert!(
        is_status(seen.last().unwrap(), RunStatus::Done),
        "{seen:#?}"
    );

    let answers: Vec<serde_json::Value> = std::fs::read_to_string(&answers)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(answers.len(), 9, "{answers:#?}");
    let refused = serde_json::json!({"Err": crewd::server::NOT_FROM_AGENTS});
    assert_eq!(answers[0]["result"], serde_json::json!({"Ok": null}));
    for (i, what) in [
        (1, "Decide"),
        (2, "StopAll"),
        (3, "CreateAgent"),
        (4, "SubscribeAll"),
        (5, "Handoff as another run"),
        (6, "PermissionAsk as another run"),
    ] {
        assert_eq!(answers[i]["result"], refused, "{what}: {answers:#?}");
    }
    // Handed off and asked like crew-mcp does.
    assert_ne!(answers[7]["result"], refused, "{answers:#?}");
    assert_eq!(
        answers[8]["result"]["Ok"]["allow"],
        serde_json::json!(true),
        "{answers:#?}"
    );
    // Nothing it tried took effect.
    let agents: Vec<crew_core::store::StoredAgent> = c
        .ok(Request::ListAgents {
            include_deleted: false,
        })
        .await;
    assert_eq!(agents.len(), 1);
}

/// Writes a script that prints `fixture` and then waits, like a CLI kept
/// alive by a background task after it answered.
fn answer_then_wait_script(dir: &Path, fixture_name: &str) -> PathBuf {
    let path = dir.join("claude-answer-wait.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat '{}'\nexec sleep 600\n",
            fixture(fixture_name).display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The bodies of the messages in `seen` not written by you, with who
/// wrote them.
fn messages(seen: &[ServerMsg]) -> Vec<(Author, String)> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Message { message, .. } if message.author != Author::You => {
                Some((message.author.clone(), message.body.clone()))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_run_still_posts_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let script = answer_then_wait_script(dir.path(), "read_file.jsonl");
    let d = Daemon::start(4, script).await;
    let mut c = d.client().await;
    let (agent, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge look at it".into(),
    })
    .await
    .unwrap();
    let seen = c
        .until(|m| {
            matches!(
                m,
                ServerMsg::Event {
                    event: Event::TurnEnd { .. },
                    ..
                }
            )
        })
        .await;
    let Some(ServerMsg::Event { run, .. }) = seen.last().cloned() else {
        unreachable!()
    };
    // Stop it before it would be ended for lingering.
    c.call(Request::StopRun { run }).await.unwrap();
    let seen = c.until(|m| is_status(m, RunStatus::Stopped)).await;
    assert_eq!(
        messages(&seen),
        vec![
            (Author::Agent(agent), fixture_summary("read_file.jsonl")),
            (Author::System, "Forge stopped.".to_string()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_stopped_by_its_limit_still_posts_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let script = answer_then_wait_script(dir.path(), "read_file.jsonl");
    // Forge's 20 min limit lasts 1 s, well before a lingering run ends.
    let d = Daemon::start_opts(ServeOptions {
        max_runs: 4,
        claude_bin: Some(script),
        minute: Duration::from_millis(50),
        ..Default::default()
    })
    .await;
    let mut c = d.client().await;
    let (agent, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge look at it".into(),
    })
    .await
    .unwrap();
    let seen = c.until(|m| is_status(m, RunStatus::Stopped)).await;
    let posted = messages(&seen);
    // Before them is the note that permission prompts are off.
    assert_eq!(
        posted[posted.len() - 2..],
        [
            (Author::Agent(agent), fixture_summary("read_file.jsonl")),
            (
                Author::System,
                "Forge hit its 20 min limit and was stopped. Its changes are kept.".to_string()
            ),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_stopped_before_it_answered_posts_no_answer() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(4, fake("claude_sleeps.sh")).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge wait".into(),
    })
    .await
    .unwrap();
    let seen = c.until(|m| is_status(m, RunStatus::Working)).await;
    let Some(ServerMsg::RunStatus { run, .. }) = seen.last().cloned() else {
        unreachable!()
    };
    c.call(Request::StopRun { run }).await.unwrap();
    let seen = c.until(|m| is_status(m, RunStatus::Stopped)).await;
    assert_eq!(
        messages(&seen),
        vec![(Author::System, "Forge stopped.".to_string())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unmentioned_reply_goes_to_the_last_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = logging_replay_script(dir.path(), "read_file.jsonl");
    let d = Daemon::start(4, script).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let mut triage = forge();
    triage.name = "Triage".into();
    let triage: AgentId = c.ok(Request::CreateAgent { spec: triage }).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();

    // Nobody ran here yet, so this one goes to no one.
    c.call(Request::PostMessage {
        thread,
        body: "hello".into(),
    })
    .await
    .unwrap();
    for body in [
        "@Forge look at it",
        "@Triage check it",
        "what's the status?",
    ] {
        c.call(Request::PostMessage {
            thread,
            body: body.into(),
        })
        .await
        .unwrap();
        c.until(|m| is_status(m, RunStatus::Done)).await;
    }

    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 3);
    let session = arg_after(&calls[1], "--session-id").unwrap();
    assert_eq!(arg_after(&calls[2], "--resume"), Some(session));
    assert_eq!(prompt_of(&calls[2]), "what's the status?");
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[2].agent, triage);
}

#[tokio::test(flavor = "multi_thread")]
async fn unmentioned_reply_waits_while_an_agent_works() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::start(4, fake("claude_sleeps.sh")).await;
    let mut c = d.client().await;
    let (_, threads) = setup(&mut c, dir.path(), 1).await;
    let thread = threads[0];
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge wait".into(),
    })
    .await
    .unwrap();
    c.until(|m| is_status(m, RunStatus::Working)).await;
    c.call(Request::PostMessage {
        thread,
        body: "one more thing".into(),
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 1, "the reply is context, not a new run");
    c.call(Request::StopThread { thread }).await.unwrap();
    c.until(|m| is_status(m, RunStatus::Stopped)).await;
}
