use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::{
    ApprovalKind, ApprovalStatus, Author, StoredApproval, StoredMessage, StoredRun, StoredThread,
};
use crew_proto::{
    ClientMsg, PROTO_VERSION, PermissionAnswer, Request, ServerMsg, read_frame, write_frame,
};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// A fake claude. It logs its arguments to `args.log` (each argument ends
/// with byte 0x1e, each call with 0x1f). A prompt that ends with `WAIT`
/// waits until the file `go` exists and then replies with the text in
/// `reply`, if there is one. Any other prompt replies "ok" at once.
fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude-fake.sh");
    let script = format!(
        r#"#!/bin/sh
for a in "$@"; do printf '%s\036' "$a" >> '{dir}/args.log'; done
printf '\037' >> '{dir}/args.log'
for last in "$@"; do :; done
reply=ok
case "$last" in
  *WAIT)
    while [ ! -e '{dir}/go' ]; do sleep 0.02; done
    if [ -f '{dir}/reply' ]; then reply=$(cat '{dir}/reply'); fi
    ;;
esac
printf '{{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":[]}}\n'
printf '{{"type":"result","subtype":"success","is_error":false,"result":"%s","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}}\n' "$reply"
"#,
        dir = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The prompt of each logged call, in order.
fn logged_prompts(dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(dir.join("args.log")).unwrap_or_default();
    text.split_terminator('\u{1f}')
        .map(|call| {
            call.split_terminator('\u{1e}')
                .next_back()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

struct Daemon {
    tmp: TempDir,
    home: PathBuf,
}

impl Daemon {
    async fn start(minute: Duration) -> Daemon {
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

    /// Lets waiting runs finish, replying with `reply`.
    fn go(&self, reply: &str) {
        std::fs::write(self.dir().join("reply"), reply).unwrap();
        std::fs::write(self.dir().join("go"), "").unwrap();
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
    /// Frames read while waiting for something else.
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

    async fn read(&mut self) -> ServerMsg {
        timeout(WAIT, read_frame(&mut self.stream))
            .await
            .expect("timed out waiting for crewd")
            .unwrap()
            .expect("connection closed")
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
            match self.read().await {
                ServerMsg::Response { id: got, result } if got == id => return result,
                other => self.pending.push(other),
            }
        }
    }

    async fn call(&mut self, req: Request) -> Result<serde_json::Value, String> {
        let id = self.send(req).await;
        self.response(id).await
    }

    async fn ok<T: serde::de::DeserializeOwned>(&mut self, req: Request) -> T {
        serde_json::from_value(self.call(req).await.unwrap()).unwrap()
    }

    /// Reads notifications until `done` returns true, and returns them all.
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
}

fn agent(name: &str, mode: PermissionMode, minutes: u32) -> AgentSpec {
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
        max_run_minutes: minutes,
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

struct Chat {
    channel: ChannelId,
    thread: ThreadId,
    forge: AgentId,
    scout: AgentId,
    triage: AgentId,
}

/// A project, a channel, a thread and three agents: Forge writes, Scout
/// and Triage only read. The client subscribes to the thread.
async fn setup(d: &Daemon, c: &mut Client, forge_minutes: u32) -> Chat {
    let root = d.dir().join("repo");
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
    let forge = c
        .ok(Request::CreateAgent {
            spec: agent("Forge", PermissionMode::EditFiles, forge_minutes),
        })
        .await;
    let scout = c
        .ok(Request::CreateAgent {
            spec: agent("Scout", PermissionMode::ReadOnly, 20),
        })
        .await;
    let triage = c
        .ok(Request::CreateAgent {
            spec: agent("Triage", PermissionMode::ReadOnly, 20),
        })
        .await;
    let thread = c
        .ok(Request::CreateThread {
            channel,
            title: "work".into(),
        })
        .await;
    c.call(Request::Subscribe { thread }).await.unwrap();
    Chat {
        channel,
        thread,
        forge,
        scout,
        triage,
    }
}

/// Asks Forge to work and returns its run once it is working.
async fn start_forge(c: &mut Client, thread: ThreadId) -> RunId {
    c.call(Request::PostMessage {
        thread,
        body: "@Forge do it WAIT".into(),
    })
    .await
    .unwrap();
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
    match seen.last() {
        Some(ServerMsg::RunStatus { run, .. }) => *run,
        _ => unreachable!(),
    }
}

async fn handoff(c: &mut Client, run: RunId, agent: &str, brief: &str) -> Result<String, String> {
    c.call(Request::Handoff {
        run,
        agent: agent.into(),
        brief: brief.into(),
    })
    .await
    .map(|v| v.as_str().expect("a text").to_string())
}

fn system_message(seen: &[ServerMsg], body: &str) -> Option<StoredMessage> {
    seen.iter().find_map(|m| match m {
        ServerMsg::Message { message, .. }
            if message.author == Author::System && message.body == body =>
        {
            Some(message.clone())
        }
        _ => None,
    })
}

/// Reads notifications until a System message with `body` arrives.
async fn until_system(c: &mut Client, body: &str) -> Vec<ServerMsg> {
    c.until(|m| system_message(std::slice::from_ref(m), body).is_some())
        .await
}

fn approvals(seen: &[ServerMsg]) -> Vec<StoredApproval> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Approval { approval, .. } => Some(approval.clone()),
            _ => None,
        })
        .collect()
}

async fn thread_row(c: &mut Client, chat: &Chat) -> StoredThread {
    let threads: Vec<StoredThread> = c
        .ok(Request::ListThreads {
            channel: chat.channel,
        })
        .await;
    threads.into_iter().find(|t| t.id == chat.thread).unwrap()
}

/// Waits for a run of `agent` to reach `status`.
async fn until_run(c: &mut Client, agent: AgentId, status: RunStatus) -> StoredRun {
    timeout(WAIT, async {
        loop {
            let runs: Vec<StoredRun> = c
                .ok(Request::ListRuns {
                    status: Some(status.clone()),
                })
                .await;
            if let Some(run) = runs.into_iter().find(|r| r.agent == agent) {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("run did not get there")
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_first_creates_pending_handoff() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    let text = handoff(&mut mcp, run, "scout", "check the edit").await;
    assert_eq!(
        text.unwrap(),
        "Asked the user to approve bringing in Scout."
    );

    let seen = until_system(&mut c, "Forge wants to bring in Scout.").await;
    let msg = system_message(&seen, "Forge wants to bring in Scout.").unwrap();
    assert_eq!(msg.run, Some(run));
    let asked = approvals(&seen);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].status, ApprovalStatus::Pending);
    assert_eq!(asked[0].run, run);
    assert_eq!(
        asked[0].kind,
        ApprovalKind::Handoff {
            from: chat.forge,
            to: chat.scout,
            brief: "check the edit".into(),
            paused: false,
        }
    );

    let listed: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(chat.thread),
        })
        .await;
    assert_eq!(listed, asked);

    // A new subscriber sees the pending approval in its backlog.
    let mut late = d.client().await;
    late.call(Request::Subscribe {
        thread: chat.thread,
    })
    .await
    .unwrap();
    let backlog = late
        .until(|m| matches!(m, ServerMsg::Approval { .. }))
        .await;
    assert_eq!(approvals(&backlog), asked);

    // Nothing ran: AskFirst waits for you.
    assert_eq!(thread_row(&mut c, &chat).await.handoffs_used, 0);

    // Declining says so and leaves nothing pending.
    let second = handoff(&mut mcp, run, "Triage", "triage it").await.unwrap();
    assert_eq!(second, "Asked the user to approve bringing in Triage.");
    let seen = until_system(&mut c, "Forge wants to bring in Triage.").await;
    let triage_ask = approvals(&seen).pop().unwrap();
    c.call(Request::Decide {
        approval: triage_ask.id,
        approve: false,
        brief: None,
    })
    .await
    .unwrap();
    let seen = until_system(&mut c, "Declined Triage.").await;
    let decided = approvals(&seen).pop().unwrap();
    assert_eq!(decided.id, triage_ask.id);
    assert_eq!(decided.status, ApprovalStatus::Declined);
    let err = c
        .call(Request::Decide {
            approval: triage_ask.id,
            approve: true,
            brief: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err, "That request was already answered.");
    let listed: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(chat.thread),
        })
        .await;
    assert_eq!(listed.len(), 1);
    c.call(Request::StopAll).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_runs_target_with_edited_brief() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    handoff(&mut mcp, run, "Scout", "check it").await.unwrap();
    let seen = until_system(&mut c, "Forge wants to bring in Scout.").await;
    let ask = approvals(&seen).pop().unwrap();
    c.call(Request::Decide {
        approval: ask.id,
        approve: true,
        brief: Some("check the edit in file.txt".into()),
    })
    .await
    .unwrap();
    let seen = until_system(&mut c, "Forge brought in Scout.").await;
    let decided = approvals(&seen).pop().unwrap();
    assert_eq!(decided.status, ApprovalStatus::Approved);

    // Scout waits behind Forge in the thread.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let queued = until_run(&mut c, chat.scout, RunStatus::Queued).await;
    assert_eq!(queued.thread, chat.thread);
    // Only Forge may have started so far.
    assert!(logged_prompts(d.dir()).iter().all(|p| p.ends_with("WAIT")));

    d.go("ok");
    until_run(&mut c, chat.scout, RunStatus::Done).await;
    let prompts = logged_prompts(d.dir());
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(
        prompts[1].ends_with("check the edit in file.txt"),
        "{prompts:?}"
    );
    // The whole thread so far comes along as context.
    assert!(prompts[1].contains("You: @Forge do it WAIT"), "{prompts:?}");

    let used = thread_row(&mut c, &chat).await.handoffs_used;
    assert_eq!(used, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_runs_then_pauses() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    c.call(Request::SetThreadHandoff {
        thread: chat.thread,
        policy: Some(HandoffPolicy::Budget),
        budget: Some(1),
    })
    .await
    .unwrap();
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    let text = handoff(&mut mcp, run, "Scout", "look").await.unwrap();
    assert_eq!(text, "Scout will pick this up after you finish.");
    let seen = until_system(&mut c, "Forge brought in Scout.").await;
    assert_eq!(
        system_message(&seen, "Forge brought in Scout.")
            .unwrap()
            .run,
        Some(run)
    );
    assert!(approvals(&seen).is_empty());
    until_run(&mut c, chat.scout, RunStatus::Queued).await;
    assert_eq!(thread_row(&mut c, &chat).await.handoffs_used, 1);

    let text = handoff(&mut mcp, run, "Triage", "sort it").await.unwrap();
    assert_eq!(
        text,
        "This thread used its handoffs. Asked the user whether to continue."
    );
    let seen = until_system(&mut c, "Paused. This thread used its 1 handoffs.").await;
    let paused = approvals(&seen).pop().expect("a pending approval");
    assert_eq!(
        paused.kind,
        ApprovalKind::Handoff {
            from: chat.forge,
            to: chat.triage,
            brief: "sort it".into(),
            paused: true,
        }
    );

    // Approving a paused handoff runs exactly that one.
    c.call(Request::Decide {
        approval: paused.id,
        approve: true,
        brief: None,
    })
    .await
    .unwrap();
    until_run(&mut c, chat.triage, RunStatus::Queued).await;
    let row = thread_row(&mut c, &chat).await;
    assert_eq!(row.handoff_budget, Some(2));
    assert_eq!(row.handoffs_used, 2);
    let text = handoff(&mut mcp, run, "Scout", "again").await.unwrap();
    assert_eq!(
        text,
        "This thread used its handoffs. Asked the user whether to continue."
    );
    c.call(Request::StopAll).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_more_resumes() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    c.call(Request::SetThreadHandoff {
        thread: chat.thread,
        policy: Some(HandoffPolicy::Budget),
        budget: Some(0),
    })
    .await
    .unwrap();
    let run = start_forge(&mut c, chat.thread).await;
    let mut mcp = d.client().await;
    let text = handoff(&mut mcp, run, "Scout", "look").await.unwrap();
    assert_eq!(
        text,
        "This thread used its handoffs. Asked the user whether to continue."
    );

    c.call(Request::AllowMoreHandoffs {
        thread: chat.thread,
        n: 2,
    })
    .await
    .unwrap();
    assert_eq!(thread_row(&mut c, &chat).await.handoff_budget, Some(2));
    for _ in 0..2 {
        let text = handoff(&mut mcp, run, "Triage", "sort").await.unwrap();
        assert_eq!(text, "Triage will pick this up after you finish.");
    }
    let text = handoff(&mut mcp, run, "Triage", "sort").await.unwrap();
    assert!(text.starts_with("This thread used its handoffs."), "{text}");

    // Without a thread budget, more is added to the resolved one.
    c.call(Request::SetThreadHandoff {
        thread: chat.thread,
        policy: Some(HandoffPolicy::Budget),
        budget: None,
    })
    .await
    .unwrap();
    c.call(Request::AllowMoreHandoffs {
        thread: chat.thread,
        n: 1,
    })
    .await
    .unwrap();
    assert_eq!(thread_row(&mut c, &chat).await.handoff_budget, Some(4));
    c.call(Request::StopAll).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_to_unknown_or_self_is_refused() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;
    let mut mcp = d.client().await;

    let err = handoff(&mut mcp, run, "Nobody", "x").await.unwrap_err();
    assert_eq!(err, "There's no agent named Nobody.");
    let err = handoff(&mut mcp, run, "forge", "x").await.unwrap_err();
    assert_eq!(err, "You can't hand off to yourself.");

    let listed: Vec<StoredApproval> = c.ok(Request::ListApprovals { thread: None }).await;
    assert!(listed.is_empty());
    assert_eq!(thread_row(&mut c, &chat).await.handoffs_used, 0);
    c.call(Request::StopAll).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_mention_fallback_creates_handoff() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;

    // Forge already handed off to Scout with the tool.
    let mut mcp = d.client().await;
    handoff(&mut mcp, run, "Scout", "check it").await.unwrap();

    d.go("Done. @Scout and @Triage please review");
    let seen = until_system(&mut c, "Forge wants to bring in Triage.").await;
    let reply = seen
        .iter()
        .find_map(|m| match m {
            ServerMsg::Message { message, .. } if message.author == Author::Agent(chat.forge) => {
                Some(message.clone())
            }
            _ => None,
        })
        .expect("Forge's reply");
    assert_eq!(reply.run, Some(run));

    let listed: Vec<StoredApproval> = c
        .ok(Request::ListApprovals {
            thread: Some(chat.thread),
        })
        .await;
    let targets: Vec<(AgentId, String)> = listed
        .iter()
        .map(|a| match &a.kind {
            ApprovalKind::Handoff { to, brief, .. } => (*to, brief.clone()),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        targets,
        vec![
            (chat.scout, "check it".to_string()),
            (
                chat.triage,
                "Done. @Scout and @Triage please review".to_string()
            ),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_ask_waits_for_decision() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    let ask = mcp
        .send(Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input: serde_json::json!({ "command": "touch x.txt" }),
        })
        .await;
    let seen = until_system(&mut c, "Forge wants to run Bash: touch x.txt").await;
    assert_eq!(
        system_message(&seen, "Forge wants to run Bash: touch x.txt")
            .unwrap()
            .run,
        Some(run)
    );
    let pending = approvals(&seen).pop().unwrap();
    assert_eq!(pending.status, ApprovalStatus::Pending);

    // The same connection keeps answering other requests meanwhile.
    let list = mcp.send(Request::ListAgents).await;
    match mcp.read().await {
        ServerMsg::Response { id, result } => {
            assert_eq!(id, list);
            assert!(result.is_ok());
        }
        other => panic!("unexpected {other:?}"),
    }

    c.call(Request::Decide {
        approval: pending.id,
        approve: true,
        brief: None,
    })
    .await
    .unwrap();
    let answer: PermissionAnswer =
        serde_json::from_value(mcp.response(ask).await.unwrap()).unwrap();
    assert!(answer.allow);
    let seen = c.until(|m| matches!(m, ServerMsg::Approval { .. })).await;
    assert_eq!(
        approvals(&seen).pop().unwrap().status,
        ApprovalStatus::Approved
    );

    // A second ask, declined.
    let ask = mcp
        .send(Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input: serde_json::json!({ "command": "rm x.txt" }),
        })
        .await;
    let seen = until_system(&mut c, "Forge wants to run Bash: rm x.txt").await;
    let pending = approvals(&seen).pop().unwrap();
    c.call(Request::Decide {
        approval: pending.id,
        approve: false,
        brief: None,
    })
    .await
    .unwrap();
    let answer: PermissionAnswer =
        serde_json::from_value(mcp.response(ask).await.unwrap()).unwrap();
    assert!(!answer.allow);
    c.call(Request::StopAll).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_run_cancels_pending_permission() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    let ask = mcp
        .send(Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input: serde_json::json!({ "command": "touch x.txt" }),
        })
        .await;
    let seen = until_system(&mut c, "Forge wants to run Bash: touch x.txt").await;
    let pending = approvals(&seen).pop().unwrap();

    c.call(Request::StopRun { run }).await.unwrap();
    let answer: PermissionAnswer =
        serde_json::from_value(mcp.response(ask).await.unwrap()).unwrap();
    assert_eq!(
        answer,
        PermissionAnswer {
            allow: false,
            message: Some("The run was stopped.".into()),
        }
    );
    let seen = c
        .until(|m| {
            matches!(
                m,
                ServerMsg::RunStatus {
                    status: RunStatus::Stopped,
                    ..
                }
            )
        })
        .await;
    let cancelled = approvals(&seen).pop().expect("the cancelled approval");
    assert_eq!(cancelled.id, pending.id);
    assert_eq!(cancelled.status, ApprovalStatus::Cancelled);
    let listed: Vec<StoredApproval> = c.ok(Request::ListApprovals { thread: None }).await;
    assert!(listed.is_empty());

    // A run that isn't live can't ask.
    let err = mcp
        .call(Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input: serde_json::json!({}),
        })
        .await
        .unwrap_err();
    assert_eq!(err, "That run isn't running.");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_time_limit_stops_run() {
    let d = Daemon::start(Duration::from_millis(100)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 1).await;
    let run = start_forge(&mut c, chat.thread).await;

    let body = "Forge hit its 1 min limit and was stopped. Its changes are kept.";
    let seen = until_system(&mut c, body).await;
    assert_eq!(system_message(&seen, body).unwrap().run, Some(run));
    let stopped = until_run(&mut c, chat.forge, RunStatus::Stopped).await;
    assert_eq!(stopped.id, run);
}
