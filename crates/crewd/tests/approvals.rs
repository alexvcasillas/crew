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
    AgentProfile, ClientMsg, Limits, PROTO_VERSION, PermissionAnswer, Request, ServerMsg,
    read_frame, write_frame,
};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// A fake claude. It logs its arguments to `args.log` (each argument ends
/// with byte 0x1e, each call with 0x1f). A prompt that ends with `WAIT`
/// waits until the file `go` exists and then replies with the text in
/// `reply`, if there is one. `STUBBORN` does the same but ignores SIGINT
/// and SIGTERM from before its call is logged, so a stop takes until
/// SIGKILL. Any other prompt replies "ok" at once.
fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude-fake.sh");
    let script = format!(
        r#"#!/bin/sh
for last in "$@"; do :; done
case "$last" in *STUBBORN) trap '' INT TERM ;; esac
for a in "$@"; do printf '%s\036' "$a" >> '{dir}/args.log'; done
printf '\037' >> '{dir}/args.log'
reply=ok
case "$last" in
  *STUBBORN)
    while [ ! -e '{dir}/go' ]; do sleep 0.02; done
    ;;
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

/// Waits until at least `n` calls are logged in full.
async fn until_logged(dir: &Path, n: usize) -> Vec<String> {
    timeout(WAIT, async {
        loop {
            let prompts = logged_prompts(dir);
            if prompts.len() >= n {
                return prompts;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("calls were not logged")
}

/// The prompt of each call logged in full, in order. A call still being
/// logged, as a busy machine may show, is left out.
fn logged_prompts(dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(dir.join("args.log")).unwrap_or_default();
    let mut calls: Vec<&str> = text.split('\u{1f}').collect();
    // What follows the last call's end is a call being logged, or nothing.
    calls.pop();
    calls
        .into_iter()
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
    start_forge_with(c, thread, "@Forge do it WAIT").await
}

async fn start_forge_with(c: &mut Client, thread: ThreadId, body: &str) -> RunId {
    c.call(Request::PostMessage {
        thread,
        body: body.into(),
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
    until_run_within(WAIT, c, agent, status).await
}

async fn until_run_within(
    wait: Duration,
    c: &mut Client,
    agent: AgentId,
    status: RunStatus,
) -> StoredRun {
    timeout(wait, async {
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
    let text = handoff(&mut mcp, run, "@scout", "check the edit").await;
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
    // Only Forge may have started so far. Its process may still be
    // logging its call when it counts as working.
    let prompts = until_logged(d.dir(), 1).await;
    assert!(prompts.iter().all(|p| p.ends_with("WAIT")), "{prompts:?}");

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
async fn handoff_to_a_deleted_agent_is_not_approved() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    handoff(&mut mcp, run, "Scout", "check it").await.unwrap();
    let seen = until_system(&mut c, "Forge wants to bring in Scout.").await;
    let ask = approvals(&seen).pop().unwrap();
    c.call(Request::DeleteAgent { agent: chat.scout })
        .await
        .unwrap();
    let err = c
        .call(Request::Decide {
            approval: ask.id,
            approve: true,
            brief: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err, "Scout was deleted. Decline this instead.");
    c.call(Request::Decide {
        approval: ask.id,
        approve: false,
        brief: None,
    })
    .await
    .unwrap();
    until_system(&mut c, "Declined Scout.").await;
    c.call(Request::StopAll).await.unwrap();
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
    // Both handoffs that ran count in the profiles, the one under the
    // budget and the approved one.
    let handoffs = |p: AgentProfile| (p.stats.handoffs_sent_week, p.stats.handoffs_received_week);
    let forge: AgentProfile = c.ok(Request::AgentProfile { agent: chat.forge }).await;
    assert_eq!(handoffs(forge), (2, 0));
    let scout: AgentProfile = c.ok(Request::AgentProfile { agent: chat.scout }).await;
    assert_eq!(handoffs(scout), (0, 1));
    let triage: AgentProfile = c.ok(Request::AgentProfile { agent: chat.triage }).await;
    assert_eq!(handoffs(triage), (0, 1));
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
            tool_use_id: None,
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
    let list = mcp
        .send(Request::ListAgents {
            include_deleted: false,
        })
        .await;
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
            tool_use_id: None,
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
            tool_use_id: None,
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
            tool_use_id: None,
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

#[tokio::test(flavor = "multi_thread")]
async fn no_run_limit_never_stops_a_run() {
    // A minute lasts 1 ms, so any limit would stop the run at once.
    let d = Daemon::start(Duration::from_millis(1)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 0).await;
    let run = start_forge(&mut c, chat.thread).await;
    until_run(&mut c, chat.forge, RunStatus::Working).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let working: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Working),
        })
        .await;
    assert!(working.iter().any(|r| r.id == run), "still working");
    c.call(Request::StopThread {
        thread: chat.thread,
    })
    .await
    .unwrap();
    until_run(&mut c, chat.forge, RunStatus::Stopped).await;
}

async fn set_budget(c: &mut Client, thread: ThreadId, budget: u32) {
    c.call(Request::SetThreadHandoff {
        thread,
        policy: Some(HandoffPolicy::Budget),
        budget: Some(budget),
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_run_refuses_handoffs_and_asks() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    set_budget(&mut c, chat.thread, 3).await;
    let run = start_forge_with(&mut c, chat.thread, "@Forge do it STUBBORN").await;
    // Wait until claude itself is up, so the stop has a process to wind down.
    timeout(WAIT, async {
        while logged_prompts(d.dir()).is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("claude did not start");

    c.call(Request::StopThread {
        thread: chat.thread,
    })
    .await
    .unwrap();
    let mut mcp = d.client().await;
    let err = handoff(&mut mcp, run, "Scout", "look").await.unwrap_err();
    assert_eq!(err, "That run isn't running.");
    let answer: PermissionAnswer = mcp
        .ok(Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input: serde_json::json!({ "command": "touch x.txt" }),
            tool_use_id: None,
        })
        .await;
    assert_eq!(
        answer,
        PermissionAnswer {
            allow: false,
            message: Some("The run was stopped.".into()),
        }
    );

    // The run was still winding down, and nothing came of either request.
    let working: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Working),
        })
        .await;
    assert!(working.iter().any(|r| r.id == run));
    let queued: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Queued),
        })
        .await;
    assert!(queued.is_empty());
    let listed: Vec<StoredApproval> = c.ok(Request::ListApprovals { thread: None }).await;
    assert!(listed.is_empty());
    assert_eq!(thread_row(&mut c, &chat).await.handoffs_used, 0);
    // Only SIGKILL, 8 s after the stop, ends it.
    until_run_within(
        Duration::from_secs(30),
        &mut c,
        chat.forge,
        RunStatus::Stopped,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_thread_drops_queued_handoff() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    set_budget(&mut c, chat.thread, 3).await;
    let run = start_forge(&mut c, chat.thread).await;
    let mut mcp = d.client().await;
    handoff(&mut mcp, run, "Scout", "look").await.unwrap();
    let queued = until_run(&mut c, chat.scout, RunStatus::Queued).await;

    c.call(Request::StopThread {
        thread: chat.thread,
    })
    .await
    .unwrap();
    let stopped = until_run(&mut c, chat.scout, RunStatus::Stopped).await;
    assert_eq!(stopped.id, queued.id);
    until_run(&mut c, chat.forge, RunStatus::Stopped).await;
    // Scout never started.
    assert!(logged_prompts(d.dir()).iter().all(|p| p.ends_with("WAIT")));
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_on_approval_counts_toward_limit() {
    let d = Daemon::start(Duration::from_secs(6)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 1).await;
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    let ask = mcp
        .send(Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input: serde_json::json!({ "command": "touch x.txt" }),
            tool_use_id: None,
        })
        .await;
    until_system(&mut c, "Forge wants to run Bash: touch x.txt").await;

    // Nobody decides, and the limit ends the run anyway.
    let answer: PermissionAnswer =
        serde_json::from_value(mcp.response(ask).await.unwrap()).unwrap();
    assert!(!answer.allow);
    until_system(
        &mut c,
        "Forge hit its 1 min limit and was stopped. Its changes are kept.",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_handoff_prompt_has_reply_once() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    set_budget(&mut c, chat.thread, 3).await;
    start_forge(&mut c, chat.thread).await;

    let reply = "Done. @Scout look at it";
    d.go(reply);
    until_run(&mut c, chat.scout, RunStatus::Done).await;
    let prompts = logged_prompts(d.dir());
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert_eq!(prompts[1].matches(reply).count(), 1, "{prompts:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_ask_connection_cancels_the_ask() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    mcp.send(Request::PermissionAsk {
        run,
        tool: "Bash".into(),
        input: serde_json::json!({ "command": "touch x.txt" }),
        tool_use_id: Some("toolu_9".into()),
    })
    .await;
    let ask_text = "Forge wants to run Bash: touch x.txt";
    let seen = until_system(&mut c, ask_text).await;
    let pending = approvals(&seen).pop().unwrap();
    assert_eq!(
        pending.kind,
        ApprovalKind::Permission {
            tool: "Bash".into(),
            input: serde_json::json!({ "command": "touch x.txt" }),
            tool_use_id: Some("toolu_9".into()),
        }
    );
    assert_eq!(
        system_message(&seen, ask_text).unwrap().approval,
        Some(pending.id)
    );

    // crew-mcp went away, or dropped the call when Claude cancelled it.
    drop(mcp);
    let gone = "Forge stopped waiting for permission.";
    let seen = until_system(&mut c, gone).await;
    let cancelled = approvals(&seen).pop().expect("the cancelled approval");
    assert_eq!(cancelled.id, pending.id);
    assert_eq!(cancelled.status, ApprovalStatus::Cancelled);
    let note = system_message(&seen, gone).unwrap();
    assert_eq!((note.run, note.approval), (Some(run), Some(pending.id)));
    let listed: Vec<StoredApproval> = c.ok(Request::ListApprovals { thread: None }).await;
    assert!(listed.is_empty());
    // The run itself goes on.
    let working: Vec<StoredRun> = c
        .ok(Request::ListRuns {
            status: Some(RunStatus::Working),
        })
        .await;
    assert!(working.iter().any(|r| r.id == run));
    c.call(Request::StopAll).await.unwrap();
}

/// Where the first frame matching `f` is in `seen`.
fn position(seen: &[ServerMsg], f: impl Fn(&ServerMsg) -> bool) -> usize {
    seen.iter().position(f).expect("frame not found")
}

#[tokio::test(flavor = "multi_thread")]
async fn approvals_come_before_their_messages_live_and_in_backlog() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let run = start_forge(&mut c, chat.thread).await;

    // A handoff ask, approved.
    let mut mcp = d.client().await;
    handoff(&mut mcp, run, "Scout", "look").await.unwrap();
    let ask_text = "Forge wants to bring in Scout.";
    let live = until_system(&mut c, ask_text).await;
    let asked = approvals(&live).pop().unwrap();
    let is_approval = |m: &ServerMsg| matches!(m, ServerMsg::Approval { approval, .. } if approval.id == asked.id);
    let is_msg = |body: &'static str| move |m: &ServerMsg| matches!(m, ServerMsg::Message { message, .. } if message.body == body);
    assert!(position(&live, is_approval) < position(&live, is_msg(ask_text)));
    c.call(Request::Decide {
        approval: asked.id,
        approve: true,
        brief: None,
    })
    .await
    .unwrap();
    let done_text = "Forge brought in Scout.";
    let live = until_system(&mut c, done_text).await;
    assert!(position(&live, is_approval) < position(&live, is_msg(done_text)));
    assert_eq!(
        system_message(&live, done_text).unwrap().approval,
        Some(asked.id)
    );

    // A late subscriber gets the decided approval once, before the first
    // message about it.
    let mut late = d.client().await;
    late.call(Request::Subscribe {
        thread: chat.thread,
    })
    .await
    .unwrap();
    late.call(Request::ListAgents {
        include_deleted: false,
    })
    .await
    .unwrap();
    let backlog = std::mem::take(&mut late.pending);
    let found: Vec<&StoredApproval> = backlog
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Approval { approval, .. } if approval.id == asked.id => Some(approval),
            _ => None,
        })
        .collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].status, ApprovalStatus::Approved);
    assert!(position(&backlog, is_approval) < position(&backlog, is_msg(ask_text)));
    c.call(Request::StopAll).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_policy_applies_when_thread_and_agent_have_none() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    c.call(Request::SetChannelHandoff {
        channel: chat.channel,
        policy: Some(HandoffPolicy::Budget),
        budget: Some(1),
    })
    .await
    .unwrap();
    let run = start_forge(&mut c, chat.thread).await;
    let mut mcp = d.client().await;
    assert_eq!(
        handoff(&mut mcp, run, "Scout", "look").await.unwrap(),
        "Scout will pick this up after you finish."
    );
    // The channel's budget of 1 is used up.
    assert_eq!(
        handoff(&mut mcp, run, "Triage", "check").await.unwrap(),
        "This thread used its handoffs. Asked the user whether to continue."
    );
    let err = c
        .call(Request::SetChannelHandoff {
            channel: ChannelId::new(),
            policy: None,
            budget: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err, "channel not found");
    c.call(Request::StopAll).await.unwrap();
}

/// The thread as the next `ThreadChanged` for it says, read from an app
/// connection that subscribed to everything.
async fn next_thread_change(app: &mut Client, thread: ThreadId) -> StoredThread {
    let seen = app
        .until(|m| matches!(m, ServerMsg::ThreadChanged { thread: t } if t.id == thread))
        .await;
    match seen.last() {
        Some(ServerMsg::ThreadChanged { thread }) => thread.clone(),
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn handoffs_used_and_worktree_changes_are_broadcast() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    let mut app = d.client().await;
    app.call(Request::SubscribeAll).await.unwrap();

    // Forge edits files, so the thread gets its worktree first.
    let run = start_forge(&mut c, chat.thread).await;
    let mut row = next_thread_change(&mut app, chat.thread).await;
    while !row.worktree.as_ref().is_some_and(|wt| wt.ready) {
        row = next_thread_change(&mut app, chat.thread).await;
    }
    assert!(row.worktree.unwrap().branch.starts_with("crew/"));

    // A handoff the budget lets run.
    set_budget(&mut c, chat.thread, 2).await;
    next_thread_change(&mut app, chat.thread).await;
    let mut mcp = d.client().await;
    handoff(&mut mcp, run, "Scout", "look").await.unwrap();
    assert_eq!(
        next_thread_change(&mut app, chat.thread)
            .await
            .handoffs_used,
        1
    );

    // An approved one.
    c.call(Request::SetThreadHandoff {
        thread: chat.thread,
        policy: Some(HandoffPolicy::AskFirst),
        budget: None,
    })
    .await
    .unwrap();
    next_thread_change(&mut app, chat.thread).await;
    handoff(&mut mcp, run, "Triage", "sort").await.unwrap();
    let seen = until_system(&mut c, "Forge wants to bring in Triage.").await;
    let ask = approvals(&seen).pop().unwrap();
    c.call(Request::Decide {
        approval: ask.id,
        approve: true,
        brief: None,
    })
    .await
    .unwrap();
    assert_eq!(
        next_thread_change(&mut app, chat.thread)
            .await
            .handoffs_used,
        2
    );

    // Discarding clears the worktree.
    c.call(Request::StopThread {
        thread: chat.thread,
    })
    .await
    .unwrap();
    until_run(&mut c, chat.forge, RunStatus::Stopped).await;
    timeout(WAIT, async {
        while c
            .call(Request::Discard {
                thread: chat.thread,
                repos: None,
                delete_branch: false,
            })
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("discard went through");
    let mut row = next_thread_change(&mut app, chat.thread).await;
    while row.worktree.is_some() {
        row = next_thread_change(&mut app, chat.thread).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn global_budget_applies_when_nothing_narrower_sets_one() {
    let d = Daemon::start(Duration::from_secs(60)).await;
    let mut c = d.client().await;
    let chat = setup(&d, &mut c, 20).await;
    c.call(Request::SetLimits {
        limits: Limits {
            handoff_policy: HandoffPolicy::Budget,
            handoff_budget: 1,
            ..Limits::default()
        },
    })
    .await
    .unwrap();
    let run = start_forge(&mut c, chat.thread).await;

    let mut mcp = d.client().await;
    let text = handoff(&mut mcp, run, "Scout", "look").await.unwrap();
    assert_eq!(text, "Scout will pick this up after you finish.");
    until_system(&mut c, "Forge brought in Scout.").await;
    let text = handoff(&mut mcp, run, "Triage", "sort it").await.unwrap();
    assert_eq!(
        text,
        "This thread used its handoffs. Asked the user whether to continue."
    );
    until_system(&mut c, "Paused. This thread used its 1 handoffs.").await;
    c.call(Request::StopAll).await.unwrap();
}
