//! Calls Claude Code's auto mode safety check stops: the Blocked approval
//! and its two answers.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{ApprovalId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::store::{ApprovalKind, ApprovalStatus, StoredApproval, StoredRun};
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// The settings of a run that may make the stopped call once.
const ALLOW_RM: &str = r#"{"permissions":{"allow":["Bash(rm -rf build)"]}}"#;

const RETRY: &str = "The safety check stopped `rm -rf build`. The user allowed it once. Run it now if you still need it, then carry on.";

/// A fake claude. It logs its arguments to `args.log` (each argument ends
/// with byte 0x1e, each call with 0x1f). A prompt with `BLOCK` in it first
/// tries `rm -rf build`, which the safety check refuses; with `COMMA` in
/// it too, `git log --format=%h,%s` instead. With `DENY` a call is denied
/// without a reason; with `QUOTE` a failed command's output quotes the
/// refusal. One that ends
/// with `WAIT` then waits until the file `go` exists.
fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude-fake.sh");
    let script = format!(
        r#"#!/bin/sh
for last in "$@"; do :; done
for a in "$@"; do printf '%s\036' "$a" >> '{dir}/args.log'; done
printf '\037' >> '{dir}/args.log'
printf '{{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":[]}}\n'
denials=''
cmd='rm -rf build'
case "$last" in
  *COMMA*) cmd='git log --format=%h,%s' ;;
esac
case "$last" in
  *BLOCK*)
    denials='{{"tool_name":"Bash","tool_input":{{"command":"'"$cmd"'"}},"tool_use_id":"toolu_01"}}'
    printf '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"toolu_01","name":"Bash","input":{{"command":"%s"}}}}]}}}}\n' "$cmd"
    printf '{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_01","is_error":true,"content":"Permission for this action has been denied by the Claude Code auto mode classifier. Reason: [Destructive or unwanted actions] Find another way."}}]}}}}\n'
    ;;
  *DENY*)
    denials='{{"tool_name":"Bash","tool_input":{{"command":"ls"}},"tool_use_id":"toolu_02"}}'
    ;;
  *QUOTE*)
    printf '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"toolu_03","name":"Bash","input":{{"command":"cat notes.txt; false"}}}}]}}}}\n'
    printf '{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_03","is_error":true,"content":"notes: Permission for this action has been denied by the Claude Code auto mode classifier. Reason: [Destructive or unwanted actions]"}}]}}}}\n'
    ;;
esac
case "$last" in
  *WAIT) while [ ! -e '{dir}/go' ]; do sleep 0.02; done ;;
esac
printf '{{"type":"result","subtype":"success","is_error":false,"result":"ok","duration_ms":1,"num_turns":1,"permission_denials":[%s],"session_id":"s"}}\n' "$denials"
"#,
        dir = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The arguments of each call logged in full, in order.
fn logged_calls(dir: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(dir.join("args.log")).unwrap_or_default();
    let mut calls: Vec<&str> = text.split('\u{1f}').collect();
    calls.pop();
    calls
        .into_iter()
        .map(|call| call.split_terminator('\u{1e}').map(String::from).collect())
        .collect()
}

async fn until_logged(dir: &Path, n: usize) -> Vec<Vec<String>> {
    timeout(WAIT, async {
        loop {
            let calls = logged_calls(dir);
            if calls.len() >= n {
                return calls;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("calls were not logged")
}

/// The value after `flag` in one call's arguments.
fn flag<'a>(call: &'a [String], flag: &str) -> Option<&'a str> {
    call.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

struct Daemon {
    tmp: TempDir,
    home: PathBuf,
}

impl Daemon {
    async fn start() -> Daemon {
        Daemon::start_with(Duration::from_secs(60)).await
    }

    /// With a time limit minute of `minute`. A stopped call is no longer
    /// needed after 1440 of them.
    async fn start_with(minute: Duration) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        let claude = fake_claude(tmp.path());
        let path = home.clone();
        tokio::spawn(async move {
            let opts = ServeOptions {
                max_runs: 4,
                claude_bin: Some(claude),
                crew_mcp_bin: None,
                minute,
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
        Daemon { tmp, home }
    }

    fn dir(&self) -> &Path {
        self.tmp.path()
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
    async fn read(&mut self) -> ServerMsg {
        timeout(WAIT, read_frame(&mut self.stream))
            .await
            .expect("timed out waiting for crewd")
            .unwrap()
            .expect("connection closed")
    }

    async fn call(&mut self, req: Request) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.stream, &ClientMsg::Request { id, req })
            .await
            .unwrap();
        loop {
            match self.read().await {
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
                self.read().await
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

    /// Waits for the run to end as `Done`.
    async fn until_done(&mut self, run: RunId) {
        self.until(|m| {
            matches!(m, ServerMsg::RunStatus { run: r, status: RunStatus::Done, .. } if *r == run)
        })
        .await;
    }

    async fn runs(&mut self, thread: ThreadId) -> Vec<StoredRun> {
        let runs: Vec<StoredRun> = self.ok(Request::ListRuns { status: None }).await;
        runs.into_iter().filter(|r| r.thread == thread).collect()
    }
}

fn agent(name: &str, mode: PermissionMode) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        role: String::new(),
        runtime: Runtime::ClaudeCode,
        model: "claude-sonnet-4-6".into(),
        instructions: String::new(),
        permission_mode: mode,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 0,
    }
}

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

/// A thread with Forge in `Auto` mode and Scout, who edits too but asks.
/// The client subscribes to the thread.
async fn setup(d: &Daemon, c: &mut Client) -> ThreadId {
    let root = d.dir().join("repo");
    git_repo(&root);
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
    for spec in [
        agent("Forge", PermissionMode::Auto),
        agent("Scout", PermissionMode::EditFiles),
    ] {
        let _: crew_core::ids::AgentId = c.ok(Request::CreateAgent { spec }).await;
    }
    let thread = c
        .ok(Request::CreateThread {
            channel,
            title: "work".into(),
        })
        .await;
    c.call(Request::Subscribe { thread }).await.unwrap();
    thread
}

/// Posts `body` and returns the Blocked approval it leads to and the run.
async fn blocked(c: &mut Client, thread: ThreadId, body: &str) -> (StoredApproval, RunId) {
    c.call(Request::PostMessage {
        thread,
        body: body.into(),
        skill: None,
        model_override: None,
    })
    .await
    .unwrap();
    let seen = c
        .until(|m| {
            matches!(m, ServerMsg::Approval { approval, .. }
                if matches!(approval.kind, ApprovalKind::Blocked { .. }))
        })
        .await;
    let Some(ServerMsg::Approval { approval, .. }) = seen.last() else {
        unreachable!()
    };
    (approval.clone(), approval.run)
}

async fn decide(c: &mut Client, approval: ApprovalId, approve: bool) {
    c.call(Request::Decide {
        approval,
        approve,
        brief: None,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_blocked_call_asks_you_and_allow_retries_it_once() {
    let d = Daemon::start().await;
    let mut c = d.client().await;
    let thread = setup(&d, &mut c).await;

    let (approval, run) = blocked(&mut c, thread, "@Forge clean up BLOCK").await;
    assert_eq!(approval.status, ApprovalStatus::Pending);
    assert_eq!(
        approval.kind,
        ApprovalKind::Blocked {
            tool: "Bash".into(),
            input: serde_json::json!({"command": "rm -rf build"}),
            reason: "Destructive or unwanted actions".into(),
        }
    );
    c.until_done(run).await;
    let first = until_logged(d.dir(), 1).await.remove(0);
    assert_eq!(flag(&first, "--permission-mode"), Some("auto"));
    let session = flag(&first, "--session-id").unwrap().to_string();

    // The run already ended: it still waits for you, and only once.
    let pending: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(thread),
        })
        .await;
    assert_eq!(pending.len(), 1);

    decide(&mut c, approval.id, true).await;
    let calls = until_logged(d.dir(), 2).await;
    let retry = &calls[1];
    assert_eq!(flag(retry, "--resume"), Some(session.as_str()));
    assert_eq!(flag(retry, "--settings"), Some(ALLOW_RM));
    assert!(retry.last().unwrap().ends_with(RETRY), "{retry:?}");
    let runs = c.runs(thread).await;
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.agent == runs[0].agent));
    let retry_run = runs.iter().find(|r| r.id != run).unwrap().id;
    c.until_done(retry_run).await;
    let pending: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(thread),
        })
        .await;
    assert!(pending.is_empty());

    // The rule was for that run only.
    c.call(Request::PostMessage {
        thread,
        body: "@Forge again".into(),
        skill: None,
        model_override: None,
    })
    .await
    .unwrap();
    let calls = until_logged(d.dir(), 3).await;
    assert_eq!(flag(&calls[2], "--settings"), None);
}

#[tokio::test]
async fn dismiss_starts_nothing() {
    let d = Daemon::start().await;
    let mut c = d.client().await;
    let thread = setup(&d, &mut c).await;

    let (approval, run) = blocked(&mut c, thread, "@Forge clean up BLOCK").await;
    c.until_done(run).await;
    decide(&mut c, approval.id, false).await;
    let seen = c
        .until(|m| {
            matches!(m, ServerMsg::Approval { approval: a, .. }
                if a.id == approval.id && a.status != ApprovalStatus::Pending)
        })
        .await;
    let Some(ServerMsg::Approval {
        approval: decided, ..
    }) = seen.last()
    else {
        unreachable!()
    };
    assert_eq!(decided.status, ApprovalStatus::Declined);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(c.runs(thread).await.len(), 1);
    assert_eq!(logged_calls(d.dir()).len(), 1);
    assert_eq!(
        c.call(Request::Decide {
            approval: approval.id,
            approve: true,
            brief: None
        })
        .await,
        Err("That request was already answered.".into())
    );
}

#[tokio::test]
async fn allow_during_the_run_retries_once_it_ends() {
    let d = Daemon::start().await;
    let mut c = d.client().await;
    let thread = setup(&d, &mut c).await;

    let (approval, run) = blocked(&mut c, thread, "@Forge clean up BLOCK WAIT").await;
    decide(&mut c, approval.id, true).await;
    let runs = c.runs(thread).await;
    let retry = runs.iter().find(|r| r.id != run).unwrap();
    assert_eq!(retry.status, RunStatus::Queued);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(logged_calls(d.dir()).len(), 1, "waits for the run");

    std::fs::write(d.dir().join("go"), "").unwrap();
    let calls = until_logged(d.dir(), 2).await;
    assert_eq!(flag(&calls[1], "--settings"), Some(ALLOW_RM));
    c.until_done(retry.id).await;
}

#[tokio::test]
async fn only_auto_agents_get_blocked_cards() {
    let d = Daemon::start().await;
    let mut c = d.client().await;
    let thread = setup(&d, &mut c).await;

    c.call(Request::PostMessage {
        thread,
        body: "@Scout clean up BLOCK".into(),
        skill: None,
        model_override: None,
    })
    .await
    .unwrap();
    until_logged(d.dir(), 1).await;
    let runs = c.runs(thread).await;
    c.until(|m| {
        matches!(m, ServerMsg::RunStatus { run, status: RunStatus::Done, .. } if *run == runs[0].id)
    })
    .await;
    let pending: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(thread),
        })
        .await;
    assert!(pending.is_empty());
}

#[tokio::test]
async fn a_call_no_rule_matches_exactly_is_never_retried() {
    let d = Daemon::start().await;
    let mut c = d.client().await;
    let thread = setup(&d, &mut c).await;

    let (approval, run) = blocked(&mut c, thread, "@Forge log COMMA BLOCK").await;
    assert_eq!(
        approval.kind,
        ApprovalKind::Blocked {
            tool: "Bash".into(),
            input: serde_json::json!({"command": "git log --format=%h,%s"}),
            reason: "Destructive or unwanted actions".into(),
        }
    );
    c.until_done(run).await;
    assert_eq!(
        c.call(Request::Decide {
            approval: approval.id,
            approve: true,
            brief: None
        })
        .await,
        Err(crew_core::claude::NO_EXACT_RULE.into())
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(c.runs(thread).await.len(), 1, "nothing was queued");
    let pending: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(thread),
        })
        .await;
    assert_eq!(pending.len(), 1, "still yours to dismiss");
    decide(&mut c, approval.id, false).await;
}

/// Waits until `approval` is no longer pending and returns it.
async fn until_decided(c: &mut Client, approval: ApprovalId) -> StoredApproval {
    let seen = c
        .until(|m| {
            matches!(m, ServerMsg::Approval { approval: a, .. }
                if a.id == approval && a.status != ApprovalStatus::Pending)
        })
        .await;
    let Some(ServerMsg::Approval { approval, .. }) = seen.last() else {
        unreachable!()
    };
    approval.clone()
}

#[tokio::test]
async fn a_later_run_of_the_agent_makes_a_stopped_call_stale() {
    let d = Daemon::start().await;
    let mut c = d.client().await;
    let thread = setup(&d, &mut c).await;

    let (approval, run) = blocked(&mut c, thread, "@Forge clean up BLOCK").await;
    c.until_done(run).await;
    // Scout's run is someone else's: the call still waits.
    c.call(Request::PostMessage {
        thread,
        body: "@Scout have a look".into(),
        skill: None,
        model_override: None,
    })
    .await
    .unwrap();
    let scout = until_logged(d.dir(), 2).await;
    assert!(scout[1].last().unwrap().contains("have a look"));
    let runs = c.runs(thread).await;
    let scout_run = runs.iter().find(|r| r.id != run).unwrap().id;
    c.until_done(scout_run).await;
    let pending: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(thread),
        })
        .await;
    assert_eq!(pending.len(), 1);

    c.call(Request::PostMessage {
        thread,
        body: "@Forge carry on".into(),
        skill: None,
        model_override: None,
    })
    .await
    .unwrap();
    let decided = until_decided(&mut c, approval.id).await;
    assert_eq!(decided.status, ApprovalStatus::Cancelled);
    let pending: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(thread),
        })
        .await;
    assert!(pending.is_empty());
}

#[tokio::test]
async fn a_stopped_call_is_stale_after_a_day() {
    // A minute lasts 1 ms, so a day is 1.44 s.
    let d = Daemon::start_with(Duration::from_millis(1)).await;
    let mut c = d.client().await;
    let thread = setup(&d, &mut c).await;

    let (approval, run) = blocked(&mut c, thread, "@Forge clean up BLOCK").await;
    c.until_done(run).await;
    let decided = until_decided(&mut c, approval.id).await;
    assert_eq!(decided.status, ApprovalStatus::Cancelled);
    assert_eq!(c.runs(thread).await.len(), 1, "nothing else ran");
}

#[tokio::test]
async fn only_a_real_safety_check_refusal_makes_a_card() {
    let d = Daemon::start().await;
    let mut c = d.client().await;
    let thread = setup(&d, &mut c).await;

    let mut seen: Vec<RunId> = Vec::new();
    for body in ["@Forge list DENY", "@Forge read QUOTE"] {
        c.call(Request::PostMessage {
            thread,
            body: body.into(),
            skill: None,
            model_override: None,
        })
        .await
        .unwrap();
        let run = c
            .runs(thread)
            .await
            .into_iter()
            .map(|r| r.id)
            .find(|id| !seen.contains(id))
            .unwrap();
        c.until_done(run).await;
        seen.push(run);
        let all: Vec<StoredApproval> = c
            .ok(Request::ListApprovals {
                thread: Some(thread),
            })
            .await;
        assert!(all.is_empty(), "{body}: {all:?}");
    }
    assert_eq!(logged_calls(d.dir()).len(), 2);
}
