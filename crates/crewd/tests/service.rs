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
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = home.clone();
        tokio::spawn(async move {
            let opts = ServeOptions {
                max_runs,
                claude_bin: Some(claude_bin),
                crew_mcp_bin,
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
    c.until(|m| is_status(m, RunStatus::Done)).await;

    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    let calls = logged_calls(&log);
    assert_eq!(calls.len(), 1);
    let args = &calls[0];
    let config: serde_json::Value =
        serde_json::from_str(&arg_after(args, "--mcp-config").expect("--mcp-config")).unwrap();
    let crew = &config["mcpServers"]["crew"];
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
        Some("mcp__crew__approve")
    );
    assert_eq!(
        arg_after(args, "--permission-mode").as_deref(),
        Some("default")
    );
    assert!(!args.iter().any(|a| a == "--permission-prompts"));
    let allowed = arg_after(args, "--allowedTools").expect("--allowedTools");
    assert!(
        allowed.split(',').any(|t| t == "mcp__crew__handoff"),
        "{allowed}"
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
        Some("mcp__crew__handoff")
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
    c.until(|m| is_status(m, RunStatus::Done)).await;

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
