use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::{Event, RunStatus, ToolKind};
use crew_core::ids::{AgentId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::store::{Author, StoredChannel, StoredProject, StoredThread};
use crew_core::worktree::Diff;
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn fake(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fake")
        .join(name)
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../crew-core/tests/fixtures/claude")
        .join(name)
}

/// A fake claude that writes `file.txt` in its working folder, then
/// replays `edit_and_test.jsonl`.
fn writing_script(dir: &Path) -> PathBuf {
    let path = dir.join("claude-writes.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec '{}' '{}'\n",
            fake("claude_writes.sh").display(),
            fixture("edit_and_test.jsonl").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A fake claude that starts a session, then waits until it is stopped.
fn session_then_sleep_script(dir: &Path) -> PathBuf {
    let path = dir.join("claude-session-sleeps.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nhead -n 1 '{}'\nsleep 60\n",
            fixture("read_file.jsonl").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn is_setup_call(m: &ServerMsg) -> bool {
    matches!(
        m,
        ServerMsg::Event { event: Event::ToolCall { name, .. }, .. } if name == "Setup"
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A repo on `main` with one commit holding `a.txt`.
fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

struct Daemon {
    tmp: TempDir,
    home: PathBuf,
}

/// `path` written relative to the current folder.
fn relative_to_cwd(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap();
    let mut rel = PathBuf::new();
    for _ in cwd.components().skip(1) {
        rel.push("..");
    }
    rel.join(path.strip_prefix("/").unwrap())
}

impl Daemon {
    async fn start(claude_bin: PathBuf) -> Daemon {
        Self::start_with(claude_bin, ServeOptions::default(), false).await
    }

    /// Starts crewd with `opts`, given a relative crew home if `relative`.
    async fn start_with(claude_bin: PathBuf, opts: ServeOptions, relative: bool) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        let path = if relative {
            relative_to_cwd(&home)
        } else {
            home.clone()
        };
        assert_eq!(path.is_relative(), relative);
        tokio::spawn(async move {
            let opts = ServeOptions {
                claude_bin: Some(claude_bin),
                crew_mcp_bin: None,
                ..opts
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
        Daemon { tmp, home }
    }

    /// The project root, a git repo next to the crew home.
    fn root(&self) -> PathBuf {
        self.tmp.path().join("root")
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
    async fn send(&mut self, req: Request) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.stream, &ClientMsg::Request { id, req })
            .await
            .unwrap();
        id
    }

    async fn call(&mut self, req: Request) -> Result<serde_json::Value, String> {
        let id = self.send(req).await;
        self.response(id).await
    }

    /// Reads until the response to request `id`, keeping other frames.
    async fn response(&mut self, id: u64) -> Result<serde_json::Value, String> {
        if let Some(pos) = self
            .pending
            .iter()
            .position(|m| matches!(m, ServerMsg::Response { id: got, .. } if *got == id))
        {
            let ServerMsg::Response { result, .. } = self.pending.remove(pos) else {
                unreachable!()
            };
            return result;
        }
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

    /// Reads notifications until `done` returns true, and returns them all.
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

    async fn until_status(&mut self, want: RunStatus) -> Vec<ServerMsg> {
        self.until(|m| matches!(m, ServerMsg::RunStatus { status, .. } if *status == want))
            .await
    }
}

fn agent(name: &str, mode: PermissionMode) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        role: "Helps".into(),
        runtime: Runtime::ClaudeCode,
        model: "claude-haiku-4-5".into(),
        instructions: String::new(),
        permission_mode: mode,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    }
}

struct Setup {
    project: ProjectId,
    channel: ChannelId,
    thread: ThreadId,
}

/// A repo at the daemon's root, a project on it, the Forge (writes) and
/// Scout (read only) agents, and a subscribed thread titled "Fix login".
async fn setup(d: &Daemon, c: &mut Client) -> Setup {
    repo(&d.root());
    let project: ProjectId = c
        .ok(Request::CreateProject {
            name: "Demo".into(),
            root: d.root().to_string_lossy().into_owned(),
            main_branch: "main".into(),
        })
        .await;
    let channel: ChannelId = c
        .ok(Request::CreateChannel {
            project,
            name: "general".into(),
        })
        .await;
    let _: AgentId = c
        .ok(Request::CreateAgent {
            spec: agent("Forge", PermissionMode::EditFiles),
        })
        .await;
    let _: AgentId = c
        .ok(Request::CreateAgent {
            spec: agent("Scout", PermissionMode::ReadOnly),
        })
        .await;
    let thread: ThreadId = c
        .ok(Request::CreateThread {
            channel,
            title: "Fix login".into(),
        })
        .await;
    c.call(Request::Subscribe { thread }).await.unwrap();
    Setup {
        project,
        channel,
        thread,
    }
}

async fn post(c: &mut Client, thread: ThreadId, body: &str) {
    c.call(Request::PostMessage {
        thread,
        body: body.into(),
    })
    .await
    .unwrap();
}

async fn thread_of(c: &mut Client, s: &Setup) -> StoredThread {
    let threads: Vec<StoredThread> = c.ok(Request::ListThreads { channel: s.channel }).await;
    threads.into_iter().find(|t| t.id == s.thread).unwrap()
}

fn system_messages(seen: &[ServerMsg]) -> Vec<String> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Message { message, .. } if message.author == Author::System => {
                Some(message.body.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn first_writing_run_creates_worktree() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    std::fs::write(d.root().join(".env"), "KEY=1\n").unwrap();
    c.call(Request::UpdateProject {
        project: s.project,
        setup_command: Some("echo hello from setup".into()),
        copy_files: vec![".env".into(), "missing.txt".into(), "nope.txt".into()],
    })
    .await
    .unwrap();

    post(&mut c, s.thread, "@Forge fix it").await;
    let seen = c.until_status(RunStatus::Done).await;

    let wt_path = d.home.join("worktrees/demo/fix-login");
    let thread = thread_of(&mut c, &s).await;
    let wt = thread.worktree.expect("thread has a worktree");
    assert_eq!(wt.path, std::fs::canonicalize(&wt_path).unwrap());
    assert_eq!(wt.branch, "crew/fix-login");
    assert_eq!(wt.base, git(&d.root(), &["rev-parse", "main"]));
    assert_eq!(
        git(&wt_path, &["branch", "--show-current"]),
        "crew/fix-login"
    );

    // The agent wrote into the worktree, not the project root.
    assert!(wt_path.join("file.txt").is_file());
    assert!(!d.root().join("file.txt").exists());
    assert_eq!(
        std::fs::read_to_string(wt_path.join(".env")).unwrap(),
        "KEY=1\n"
    );
    assert!(
        system_messages(&seen).contains(&"Skipped missing files: missing.txt, nope.txt.".into()),
        "system messages: {:?}",
        system_messages(&seen)
    );

    // Setup output comes first in the run's events, in seq order.
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
        .unwrap();
    let events: Vec<(usize, u64, Event)> = seen
        .iter()
        .enumerate()
        .filter_map(|(i, m)| match m {
            ServerMsg::Event { seq, event, .. } => Some((i, *seq, event.clone())),
            _ => None,
        })
        .collect();
    assert!(working < events[0].0);
    let seqs: Vec<u64> = events.iter().map(|(_, s, _)| *s).collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    match &events[0].2 {
        Event::ToolCall {
            name, kind, target, ..
        } => {
            assert_eq!(name, "Setup");
            assert_eq!(*kind, ToolKind::Run);
            assert_eq!(target.as_deref(), Some("echo hello from setup"));
        }
        other => panic!("first event is {other:?}"),
    }
    assert!(events.iter().any(|(_, _, e)| matches!(
        e,
        Event::ToolResult { output, is_error: false, .. } if output == "hello from setup"
    )));
    assert!(
        events
            .iter()
            .any(|(_, _, e)| matches!(e, Event::SessionStarted { .. }))
    );

    // The list requests see what was made.
    let projects: Vec<StoredProject> = c.ok(Request::ListProjects).await;
    assert_eq!(projects.len(), 1);
    assert_eq!(
        projects[0].setup_command.as_deref(),
        Some("echo hello from setup")
    );
    assert_eq!(projects[0].copy_files.len(), 3);
    let channels: Vec<StoredChannel> = c.ok(Request::ListChannels { project: s.project }).await;
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].id, s.channel);

    // A second run reuses the worktree without running setup again.
    post(&mut c, s.thread, "@Forge again").await;
    let seen = c.until_status(RunStatus::Done).await;
    assert!(!seen.iter().any(|m| matches!(
        m,
        ServerMsg::Event { event: Event::ToolCall { name, .. }, .. } if name == "Setup"
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn readonly_agent_uses_project_root() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;

    post(&mut c, s.thread, "@Scout look around").await;
    c.until_status(RunStatus::Done).await;
    assert!(d.root().join("file.txt").is_file());
    assert!(!d.home.join("worktrees").exists());
    assert!(thread_of(&mut c, &s).await.worktree.is_none());

    // Once the thread has a worktree, read only agents run there too.
    std::fs::remove_file(d.root().join("file.txt")).unwrap();
    post(&mut c, s.thread, "@Forge build it").await;
    c.until_status(RunStatus::Done).await;
    let wt_path = d.home.join("worktrees/demo/fix-login");
    std::fs::remove_file(wt_path.join("file.txt")).unwrap();
    post(&mut c, s.thread, "@Scout check it").await;
    c.until_status(RunStatus::Done).await;
    assert!(wt_path.join("file.txt").is_file());
    assert!(!d.root().join("file.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_failure_fails_run_and_keeps_worktree() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    c.call(Request::UpdateProject {
        project: s.project,
        setup_command: Some(
            "i=1; while [ $i -le 25 ]; do echo line$i; i=$((i+1)); done; echo oops >&2; exit 3"
                .into(),
        ),
        copy_files: vec![],
    })
    .await
    .unwrap();

    post(&mut c, s.thread, "@Forge fix it").await;
    let seen = c.until_status(RunStatus::Failed).await;

    let failure = system_messages(&seen)
        .into_iter()
        .find(|m| m.starts_with("Forge failed:"))
        .expect("a failure message");
    let lines: Vec<&str> = failure.lines().collect();
    assert!(lines.contains(&"oops"), "{failure}");
    assert!(lines.contains(&"line25"), "{failure}");
    assert!(lines.contains(&"line7"), "{failure}");
    assert!(
        !lines.contains(&"line6"),
        "only the last 20 lines: {failure}"
    );

    // The last setup event says how it ended.
    let last_setup = seen
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Event {
                event: e @ Event::ToolResult { .. },
                ..
            } => Some(e.clone()),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(
        last_setup,
        Event::ToolResult {
            id: "setup".into(),
            is_error: true,
            output: "The setup command exited with code 3.".into(),
        }
    );

    // The worktree stays, not ready, and claude never ran in it.
    let wt_path = d.home.join("worktrees/demo/fix-login");
    assert!(wt_path.is_dir());
    assert!(!wt_path.join("file.txt").exists());
    let wt = thread_of(&mut c, &s).await.worktree.unwrap();
    assert!(!wt.ready);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_setup_runs_again_next_time() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    std::fs::write(d.root().join(".env"), "KEY=1\n").unwrap();
    std::fs::write(d.root().join(".env.dev"), "DEV=1\n").unwrap();
    // Fails the first time, works after that.
    let marker = scripts.path().join("tried");
    c.call(Request::UpdateProject {
        project: s.project,
        setup_command: Some(format!(
            "if [ -f '{m}' ]; then echo second try; else touch '{m}'; exit 1; fi",
            m = marker.display()
        )),
        copy_files: vec![".env".into(), ".env.dev".into()],
    })
    .await
    .unwrap();

    post(&mut c, s.thread, "@Forge fix it").await;
    c.until_status(RunStatus::Failed).await;
    let wt_path = d.home.join("worktrees/demo/fix-login");
    // The retry copies what is missing and keeps what is there.
    std::fs::remove_file(wt_path.join(".env")).unwrap();
    std::fs::write(wt_path.join(".env.dev"), "EDITED=1\n").unwrap();

    post(&mut c, s.thread, "@Forge try again").await;
    let seen = c.until_status(RunStatus::Done).await;
    assert!(seen.iter().any(is_setup_call), "setup should run again");
    assert!(seen.iter().any(|m| matches!(
        m,
        ServerMsg::Event { event: Event::ToolResult { output, .. }, .. } if output == "second try"
    )));
    // Same worktree, missing files copied again, and now ready.
    let wt = thread_of(&mut c, &s).await.worktree.unwrap();
    assert_eq!(wt.path, std::fs::canonicalize(&wt_path).unwrap());
    assert!(wt.ready);
    assert!(wt_path.join(".env").is_file());
    assert_eq!(
        std::fs::read_to_string(wt_path.join(".env.dev")).unwrap(),
        "EDITED=1\n"
    );
    assert!(wt_path.join("file.txt").is_file());

    // Ready: no setup on later runs.
    post(&mut c, s.thread, "@Forge once more").await;
    let seen = c.until_status(RunStatus::Done).await;
    assert!(!seen.iter().any(is_setup_call));
}

#[tokio::test(flavor = "multi_thread")]
async fn copied_files_stay_out_of_commits() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    std::fs::write(d.root().join(".env.local"), "SECRET=1\n").unwrap();
    let root_exclude = d.root().join(".git/info/exclude");
    let exclude_before = std::fs::read(&root_exclude).ok();
    c.call(Request::UpdateProject {
        project: s.project,
        setup_command: None,
        copy_files: vec![".env.local".into()],
    })
    .await
    .unwrap();

    post(&mut c, s.thread, "@Forge fix it").await;
    c.until_status(RunStatus::Done).await;
    let wt_path = d.home.join("worktrees/demo/fix-login");
    assert!(wt_path.join(".env.local").is_file());

    let diff: Diff = c.ok(Request::ThreadDiff { thread: s.thread }).await;
    let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["file.txt"]);

    let _: String = c
        .ok(Request::Commit {
            thread: s.thread,
            message: "Add file".into(),
        })
        .await;
    let committed = git(&wt_path, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(committed, "file.txt");
    assert!(wt_path.join(".env.local").is_file());
    // The user's repo is left alone.
    assert_eq!(std::fs::read(&root_exclude).ok(), exclude_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_during_setup_kills_it() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    c.call(Request::UpdateProject {
        project: s.project,
        setup_command: Some("echo started; sleep 60".into()),
        copy_files: vec![],
    })
    .await
    .unwrap();

    post(&mut c, s.thread, "@Forge fix it").await;
    let mut run: Option<RunId> = None;
    c.until(|m| match m {
        ServerMsg::RunStatus { run: r, .. } => {
            run = Some(*r);
            false
        }
        ServerMsg::Event {
            event: Event::ToolResult { output, .. },
            ..
        } => output == "started",
        _ => false,
    })
    .await;
    let started = std::time::Instant::now();
    c.call(Request::StopRun { run: run.unwrap() })
        .await
        .unwrap();
    let seen = c.until_status(RunStatus::Stopped).await;
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(system_messages(&seen).contains(&"Forge stopped.".into()));
    let wt_path = d.home.join("worktrees/demo/fix-login");
    assert!(!wt_path.join("file.txt").exists());
    assert!(!thread_of(&mut c, &s).await.worktree.unwrap().ready);

    // The stopped setup runs again on the next writing run.
    c.call(Request::UpdateProject {
        project: s.project,
        setup_command: Some("echo quick".into()),
        copy_files: vec![],
    })
    .await
    .unwrap();
    post(&mut c, s.thread, "@Forge again").await;
    let seen = c.until_status(RunStatus::Done).await;
    assert!(seen.iter().any(is_setup_call));
    assert!(wt_path.join("file.txt").is_file());
    assert!(thread_of(&mut c, &s).await.worktree.unwrap().ready);
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_then_commit() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;

    // No worktree yet: an empty diff.
    let diff: Diff = c.ok(Request::ThreadDiff { thread: s.thread }).await;
    assert_eq!(diff, Diff::default());

    post(&mut c, s.thread, "@Forge fix it").await;
    c.until_status(RunStatus::Done).await;

    let diff: Diff = c.ok(Request::ThreadDiff { thread: s.thread }).await;
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "file.txt");
    assert_eq!(diff.files[0].status, 'A');
    assert!(diff.patch.contains("+written by the agent"));

    let sha: String = c
        .ok(Request::Commit {
            thread: s.thread,
            message: "Add file".into(),
        })
        .await;
    let wt_path = d.home.join("worktrees/demo/fix-login");
    assert_eq!(git(&wt_path, &["rev-parse", "HEAD"]), sha);
    assert_eq!(git(&wt_path, &["log", "-1", "--format=%s"]), "Add file");
    assert!(wt_path.is_dir(), "the worktree stays after a commit");
    // The project root is untouched.
    assert!(!d.root().join("file.txt").exists());

    let diff: Diff = c.ok(Request::ThreadDiff { thread: s.thread }).await;
    assert_eq!(diff, Diff::default());

    // New work shows up again.
    std::fs::write(wt_path.join("a.txt"), "one\ntwo\n").unwrap();
    let diff: Diff = c.ok(Request::ThreadDiff { thread: s.thread }).await;
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "a.txt");
    assert_eq!(diff.files[0].status, 'M');

    let err = c
        .call(Request::Commit {
            thread: s.thread,
            message: "  ".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err, "Write a commit message.");
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_removes_worktree() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;

    let err = c
        .call(Request::Discard { thread: s.thread })
        .await
        .unwrap_err();
    assert_eq!(err, "This thread has no worktree.");

    post(&mut c, s.thread, "@Forge fix it").await;
    c.until_status(RunStatus::Done).await;
    let wt_path = d.home.join("worktrees/demo/fix-login");
    assert!(wt_path.is_dir());

    c.call(Request::Discard { thread: s.thread }).await.unwrap();
    assert!(!wt_path.exists());
    assert_eq!(git(&d.root(), &["branch", "--list", "crew/fix-login"]), "");
    assert!(thread_of(&mut c, &s).await.worktree.is_none());
    let diff: Diff = c.ok(Request::ThreadDiff { thread: s.thread }).await;
    assert_eq!(diff, Diff::default());

    // The next writing run starts over with a fresh worktree.
    post(&mut c, s.thread, "@Forge again").await;
    c.until_status(RunStatus::Done).await;
    assert!(wt_path.join("file.txt").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_refused_while_running() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(session_then_sleep_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;

    post(&mut c, s.thread, "@Forge wait").await;
    // Claude itself is running, past setup.
    c.until(|m| {
        matches!(
            m,
            ServerMsg::Event {
                event: Event::SessionStarted { .. },
                ..
            }
        )
    })
    .await;

    let wait = "Wait for Forge to finish, or stop it.";
    let err = c
        .call(Request::Commit {
            thread: s.thread,
            message: "Too soon".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err, wait);
    let err = c
        .call(Request::Discard { thread: s.thread })
        .await
        .unwrap_err();
    assert_eq!(err, wait);
    // Discard was refused, so the worktree stays.
    assert!(d.home.join("worktrees/demo/fix-login").is_dir());

    c.call(Request::StopThread { thread: s.thread })
        .await
        .unwrap();
    c.until_status(RunStatus::Stopped).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_keeps_a_branch_with_commits() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    post(&mut c, s.thread, "@Forge fix it").await;
    c.until_status(RunStatus::Done).await;
    let _: String = c
        .ok(Request::Commit {
            thread: s.thread,
            message: "Add file".into(),
        })
        .await;

    c.call(Request::Discard { thread: s.thread }).await.unwrap();
    let wt_path = d.home.join("worktrees/demo/fix-login");
    assert!(!wt_path.exists());
    assert!(thread_of(&mut c, &s).await.worktree.is_none());
    // The committed work is still on its branch.
    assert_eq!(
        git(&d.root(), &["branch", "--list", "crew/fix-login"]),
        "crew/fix-login"
    );
    assert_eq!(
        git(&d.root(), &["log", "-1", "--format=%s", "crew/fix-login"]),
        "Add file"
    );
    let note = "Kept branch crew/fix-login because it has commits that aren't merged.";
    let seen = c
        .until(|m| matches!(m, ServerMsg::Message { message, .. } if message.body == note))
        .await;
    assert!(system_messages(&seen).contains(&note.to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_commit_does_not_hold_up_other_requests() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_script(scripts.path())).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    post(&mut c, s.thread, "@Forge fix it").await;
    c.until_status(RunStatus::Done).await;
    let hook = d.root().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nsleep 3\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let commit = c
        .send(Request::Commit {
            thread: s.thread,
            message: "Add file".into(),
        })
        .await;
    // Let the commit get as far as the hook.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let stop = c.send(Request::StopAll).await;
    let first = timeout(WAIT, async {
        loop {
            match c.recv().await {
                ServerMsg::Response { id, .. } => return id,
                _ => continue,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(first, stop, "StopAll waited for the commit");
    assert!(started.elapsed() < Duration::from_secs(2));
    let sha: String = serde_json::from_value(c.response(commit).await.unwrap()).unwrap();
    let wt_path = d.home.join("worktrees/demo/fix-login");
    assert_eq!(git(&wt_path, &["rev-parse", "HEAD"]), sha);
}

#[tokio::test(flavor = "multi_thread")]
async fn relative_crew_home_keeps_worktrees_in_the_home() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start_with(
        writing_script(scripts.path()),
        ServeOptions::default(),
        true,
    )
    .await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    post(&mut c, s.thread, "@Forge fix it").await;
    c.until_status(RunStatus::Done).await;

    let wt = thread_of(&mut c, &s).await.worktree.unwrap();
    let home = std::fs::canonicalize(&d.home).unwrap();
    assert_eq!(wt.path, home.join("worktrees/demo/fix-login"));
    assert!(wt.path.join("file.txt").is_file());
    assert!(!d.root().join("file.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn lagging_subscriber_is_told_to_resync() {
    let scripts = tempfile::tempdir().unwrap();
    let opts = ServeOptions {
        notice_buffer: 2,
        ..ServeOptions::default()
    };
    let d = Daemon::start_with(writing_script(scripts.path()), opts, false).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;

    // `c` doesn't read, so crewd blocks writing the first big message to
    // it while the rest pile up.
    let mut poster = d.client().await;
    let big = "x".repeat(1 << 20);
    for _ in 0..5 {
        post(&mut poster, s.thread, &big).await;
    }
    // A connection that never subscribed never falls behind.
    let _: serde_json::Value = poster.ok(Request::ListAgents).await;

    loop {
        if matches!(c.recv().await, ServerMsg::Resync) {
            break;
        }
    }
    let after = timeout(WAIT, read_frame::<_, ServerMsg>(&mut c.stream))
        .await
        .expect("crewd kept the connection open");
    assert!(matches!(after, Ok(None) | Err(_)), "{after:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn time_limit_starts_after_setup() {
    let scripts = tempfile::tempdir().unwrap();
    let opts = ServeOptions {
        minute: Duration::from_secs(1),
        ..ServeOptions::default()
    };
    let d = Daemon::start_with(session_then_sleep_script(scripts.path()), opts, false).await;
    let mut c = d.client().await;
    let s = setup(&d, &mut c).await;
    let quick = AgentSpec {
        max_run_minutes: 1,
        ..agent("Quick", PermissionMode::EditFiles)
    };
    let _: AgentId = c.ok(Request::CreateAgent { spec: quick }).await;
    c.call(Request::UpdateProject {
        project: s.project,
        setup_command: Some("sleep 2".into()),
        copy_files: vec![],
    })
    .await
    .unwrap();

    post(&mut c, s.thread, "@Quick go").await;
    let limit = "Quick hit its 1 min limit and was stopped. Its changes are kept.";
    let seen = c
        .until(|m| matches!(m, ServerMsg::Message { message, .. } if message.body == limit))
        .await;
    // Claude got to start: the setup's 2 s didn't count.
    assert!(seen.iter().any(|m| matches!(
        m,
        ServerMsg::Event {
            event: Event::SessionStarted { .. },
            ..
        }
    )));
}
