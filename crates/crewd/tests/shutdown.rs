use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_core::store::{RunFilter, Store, StoredRun};
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(8);

/// Writes a fake claude that saves its pid to `pid_file`, then sleeps.
fn sleeping_script(dir: &Path, pid_file: &Path) -> PathBuf {
    let path = dir.join("claude-sleeps.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho $$ > '{}'\nsleep 60\n", pid_file.display()),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A client that only sends requests and skips notifications.
struct Client {
    stream: UnixStream,
    next_id: u64,
}

impl Client {
    async fn connect(sock: &Path) -> Client {
        let stream = timeout(WAIT, async {
            loop {
                if let Ok(s) = UnixStream::connect(sock).await {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("crewd did not start");
        let mut c = Client { stream, next_id: 1 };
        c.call(Request::Hello {
            version: PROTO_VERSION,
        })
        .await;
        c
    }

    async fn call(&mut self, req: Request) -> serde_json::Value {
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
            if let ServerMsg::Response { id: got, result } = msg
                && got == id
            {
                return result.unwrap();
            }
        }
    }

    async fn ok<T: serde::de::DeserializeOwned>(&mut self, req: Request) -> T {
        serde_json::from_value(self.call(req).await).unwrap()
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

/// Creates a project in a new repo under `dir`, the Forge agent and a
/// thread, then asks Forge to work.
async fn start_long_run(c: &mut Client, dir: &Path) -> ThreadId {
    let root = dir.join("root");
    git_repo(&root);
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
    let _: AgentId = c
        .ok(Request::CreateAgent {
            spec: AgentSpec {
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
            },
        })
        .await;
    let thread: ThreadId = c
        .ok(Request::CreateThread {
            channel,
            title: "t".into(),
        })
        .await;
    let _: crew_core::ids::MessageId = c
        .ok(Request::PostMessage {
            thread,
            body: "@Forge wait".into(),
        })
        .await;
    thread
}

/// Waits until the fake claude has written its pid.
async fn agent_pid(pid_file: &Path) -> Pid {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(pid) = std::fs::read_to_string(pid_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return Pid::from_raw(pid);
        }
        assert!(Instant::now() < deadline, "the agent never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Asserts that the agent's process group is gone, then cleans up if not.
async fn assert_group_gone(pgid: Pid) {
    // Killed children are reaped by init, which can take a moment.
    let deadline = Instant::now() + Duration::from_secs(2);
    while killpg(pgid, None) != Err(Errno::ESRCH) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let gone = killpg(pgid, None) == Err(Errno::ESRCH);
    if !gone {
        let _ = killpg(pgid, Signal::SIGKILL);
    }
    assert!(gone, "the agent outlived crewd");
}

fn runs_of(home: &Path, thread: ThreadId) -> Vec<StoredRun> {
    let store = Store::open(&home.join("crew.db")).unwrap();
    store
        .runs(RunFilter {
            status: None,
            thread: Some(thread),
        })
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_stops_agents_and_marks_runs_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    let pid_file = tmp.path().join("agent.pid");
    let mut crewd = tokio::process::Command::new(env!("CARGO_BIN_EXE_crewd"))
        .env("CREW_HOME", &home)
        .env("CREW_CLAUDE_BIN", sleeping_script(tmp.path(), &pid_file))
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut c = Client::connect(&home.join("crewd.sock")).await;
    let thread = start_long_run(&mut c, tmp.path()).await;
    let pgid = agent_pid(&pid_file).await;

    let crewd_pid = Pid::from_raw(crewd.id().unwrap() as i32);
    kill(crewd_pid, Signal::SIGTERM).unwrap();
    let status = timeout(Duration::from_secs(12), crewd.wait())
        .await
        .expect("crewd should exit after SIGTERM")
        .unwrap();

    assert_group_gone(pgid).await;
    assert!(status.success(), "crewd exited with {status}");
    let runs = runs_of(&home, thread);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Interrupted);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_hook_stops_agents_and_marks_runs_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    let pid_file = tmp.path().join("agent.pid");
    let opts = crewd::ServeOptions {
        max_runs: 1,
        claude_bin: Some(sleeping_script(tmp.path(), &pid_file)),
        crew_mcp_bin: None,
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_home = home.clone();
    let server = tokio::spawn(async move {
        crewd::serve_until(&serve_home, opts, async {
            let _ = stop_rx.await;
        })
        .await
    });

    let mut c = Client::connect(&home.join("crewd.sock")).await;
    let thread = start_long_run(&mut c, tmp.path()).await;
    let pgid = agent_pid(&pid_file).await;
    // Cap 1: this one waits in the queue.
    let _: crew_core::ids::MessageId = c
        .ok(Request::PostMessage {
            thread,
            body: "@Forge next".into(),
        })
        .await;

    stop_tx.send(()).unwrap();
    let result = timeout(Duration::from_secs(12), server)
        .await
        .expect("serve_until should return after shutdown")
        .unwrap();

    assert_group_gone(pgid).await;
    result.unwrap();
    let runs = runs_of(&home, thread);
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter().all(|r| r.status == RunStatus::Interrupted),
        "runs: {runs:?}"
    );
    assert!(runs.iter().all(|r| r.ended_at.is_some()));
    assert!(
        !home.join("crewd.sock").exists(),
        "the socket should be removed"
    );
}
