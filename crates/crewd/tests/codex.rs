//! Codex agents run through a scripted stand-in for `codex app-server`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::{Event, RunStatus};
use crew_core::ids::{ApprovalId, RunId, ThreadId};
use crew_core::store::{ApprovalKind, ApprovalStatus, Author, StoredApproval, StoredRun};
use crew_proto::{
    ClientMsg, PROTO_VERSION, Request, RuntimeInfo, ServerMsg, read_frame, write_frame,
};
use crewd::{ServeOptions, serve};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Generous, for a loaded CI runner.
const WAIT: Duration = Duration::from_secs(30);
/// The Codex thread the fake starts.
const TID: &str = "00000000-0000-7000-8000-000000000001";
const SIGNED_OUT: &str = "Forge failed: Codex isn't signed in. Run `codex login`.";
const NOT_INSTALLED: &str = "Forge failed: Couldn't start `codex`. Is Codex installed?";

fn fake_app_server() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crew-core/tests/fake/codex_app_server.py")
}

/// A `codex` that logs its arguments to `args.log`, answers `login status`
/// with `login_exit` and runs `scenario-<n>.json` for its n-th app-server.
fn codex_script(dir: &Path, login_exit: i32) -> PathBuf {
    let path = dir.join("codex");
    let d = dir.display();
    let body = format!(
        "#!/bin/sh\n\
         echo \"$*\" >> '{d}/args.log'\n\
         case \"$1\" in\n\
         --version) echo 'codex-cli 0.165.0' ;;\n\
         login) exit {login_exit} ;;\n\
         app-server)\n\
           n=$(cat '{d}/count' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{d}/count'\n\
           exec python3 '{fake}' '{d}/scenario-'$n'.json' ;;\n\
         *) exit 2 ;;\n\
         esac\n",
        fake = fake_app_server().display(),
    );
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Writes the n-th app-server's scenario. Every client message it reads
/// goes to `messages-<n>.log`.
fn scenario(dir: &Path, n: u32, steps: Vec<Value>) {
    let log = dir.join(format!("messages-{n}.log"));
    let body = json!({"log": log, "steps": steps});
    std::fs::write(dir.join(format!("scenario-{n}.json")), body.to_string()).unwrap();
}

/// Like [`scenario`] with extra top-level keys.
fn scenario_with(dir: &Path, n: u32, steps: Vec<Value>, extra: Value) {
    let log = dir.join(format!("messages-{n}.log"));
    let mut body = json!({"log": log, "steps": steps});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    std::fs::write(dir.join(format!("scenario-{n}.json")), body.to_string()).unwrap();
}

/// The client messages the n-th app-server read.
fn messages(dir: &Path, n: u32) -> Vec<Value> {
    std::fs::read_to_string(dir.join(format!("messages-{n}.log")))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn find<'a>(msgs: &'a [Value], method: &str) -> &'a Value {
    msgs.iter()
        .find(|m| m["method"] == method)
        .unwrap_or_else(|| panic!("no {method} in {msgs:?}"))
}

fn args_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("args.log")).unwrap_or_default()
}

/// Handshake and turn start. `opening` is `thread/start` or `thread/resume`.
fn opening(opening: &str, matching: Value) -> Vec<Value> {
    vec![
        json!({"expect": "initialize", "as": "init"}),
        json!({"respond": "init", "result": {"userAgent": "fake", "codexHome": "/work/codex", "platformFamily": "unix", "platformOs": "macos"}}),
        json!({"expect": "initialized"}),
        json!({"expect": opening, "as": "open", "match": matching}),
        json!({"respond": "open", "result": {"thread": {"id": TID, "cwd": "/work/demo", "path": null}, "model": "gpt-test"}}),
        json!({"send": {"method": "thread/started", "params": {"thread": {"id": TID, "model": "gpt-test", "cwd": "/work/demo"}}}}),
        json!({"expect": "turn/start", "as": "turn", "match": {"threadId": TID}}),
        json!({"respond": "turn", "result": {"turn": {"id": "turn-1", "status": "inProgress"}}}),
    ]
}

fn start() -> Vec<Value> {
    opening("thread/start", json!({}))
}

fn notify(method: &str, params: Value) -> Value {
    json!({"send": {"method": method, "params": params}})
}

fn item(method: &str, item: Value) -> Value {
    notify(
        method,
        json!({"item": item, "threadId": TID, "turnId": "turn-1"}),
    )
}

fn command(id: &str, cmd: &str, status: &str) -> Value {
    json!({"type": "commandExecution", "id": id, "command": cmd, "cwd": "/work/demo", "status": status, "commandActions": [], "aggregatedOutput": if status == "completed" { json!("ok\n") } else { Value::Null }, "exitCode": if status == "completed" { json!(0) } else { Value::Null }})
}

/// A final answer, then the end of the turn.
fn finish(text: &str) -> Vec<Value> {
    vec![
        item(
            "item/completed",
            json!({"type": "agentMessage", "id": "msg-1", "text": text, "phase": "final_answer"}),
        ),
        notify(
            "turn/completed",
            json!({"threadId": TID, "turn": {"id": "turn-1", "status": "completed", "items": [], "durationMs": 5}}),
        ),
    ]
}

fn approval_params(item_id: &str, cmd: &str) -> Value {
    json!({"threadId": TID, "turnId": "turn-1", "itemId": item_id, "command": cmd, "cwd": "/work/demo", "commandActions": []})
}

struct Daemon {
    tmp: TempDir,
    home: PathBuf,
}

impl Daemon {
    async fn start(codex_bin: PathBuf, crew_mcp: Option<PathBuf>) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        let path = home.clone();
        tokio::spawn(async move {
            let opts = ServeOptions {
                claude_bin: Some(PathBuf::from("/nonexistent/claude")),
                codex_bin: Some(codex_bin),
                crew_mcp_bin: crew_mcp,
                runtimes_wait: Some(WAIT),
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

    async fn call(&mut self, req: Request) -> Result<Value, String> {
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

    /// Reads until `run` ends and returns everything seen.
    async fn until_ended(&mut self, run: RunId) -> Vec<ServerMsg> {
        self.until(|m| {
            matches!(m, ServerMsg::RunStatus { run: r, status, .. }
                if *r == run && !matches!(status, RunStatus::Queued | RunStatus::Working))
        })
        .await
    }

    /// Reads until a pending approval arrives.
    async fn until_asked(&mut self) -> StoredApproval {
        let seen = self
            .until(|m| {
                matches!(m, ServerMsg::Approval { approval, .. }
                    if approval.status == ApprovalStatus::Pending)
            })
            .await;
        match seen.last() {
            Some(ServerMsg::Approval { approval, .. }) => approval.clone(),
            _ => unreachable!(),
        }
    }

    async fn decide(&mut self, approval: ApprovalId, approve: bool) {
        self.call(Request::Decide {
            approval,
            approve,
            brief: None,
        })
        .await
        .unwrap();
    }
}

fn forge(mode: PermissionMode) -> AgentSpec {
    AgentSpec {
        name: "Forge".into(),
        role: "Builds things".into(),
        runtime: Runtime::Codex,
        model: "gpt-test".into(),
        instructions: "Be brief.".into(),
        permission_mode: mode,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
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

/// A project in `root` (a git repo), a channel, Forge and a thread the
/// client is subscribed to.
async fn setup(c: &mut Client, root: &Path, mode: PermissionMode) -> ThreadId {
    git_repo(root);
    let project = c
        .ok(Request::CreateProject {
            name: "demo".into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
        })
        .await;
    let channel = c
        .ok(Request::CreateChannel {
            project,
            name: "general".into(),
        })
        .await;
    let _: crew_core::ids::AgentId = c.ok(Request::CreateAgent { spec: forge(mode) }).await;
    let thread = c
        .ok(Request::CreateThread {
            channel,
            title: "work".into(),
        })
        .await;
    c.call(Request::Subscribe { thread }).await.unwrap();
    thread
}

/// Posts `body` and returns the run it started.
async fn ask_forge(c: &mut Client, thread: ThreadId, body: &str) -> RunId {
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
                    status: RunStatus::Queued,
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

fn events_of(seen: &[ServerMsg], run: RunId) -> Vec<Event> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Event { run: r, event, .. } if *r == run => Some(event.clone()),
            _ => None,
        })
        .collect()
}

fn status_of(seen: &[ServerMsg]) -> RunStatus {
    match seen.last() {
        Some(ServerMsg::RunStatus { status, .. }) => status.clone(),
        other => panic!("not a status: {other:?}"),
    }
}

fn system_texts(seen: &[ServerMsg]) -> Vec<String> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Message { message, .. } if message.author == Author::System => {
                Some(message.body.clone())
            }
            _ => None,
        })
        .collect()
}

fn agent_texts(seen: &[ServerMsg]) -> Vec<String> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Message { message, .. } if matches!(message.author, Author::Agent(_)) => {
                Some(message.body.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_run_streams_events_and_ends_done() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut steps = start();
    steps.extend([
        item("item/started", command("exec-1", "ls", "inProgress")),
        item("item/completed", command("exec-1", "ls", "completed")),
        // Crew doesn't answer questions for the user.
        json!({"request": "item/tool/requestUserInput", "id": 7, "params": {}, "replyError": {"code": -32601}}),
    ]);
    steps.extend(finish("All done."));
    scenario(dir, 1, steps);
    let d = Daemon::start(codex_script(dir, 0), None).await;
    let mut c = d.client().await;
    let root = d.tmp.path().join("repo");
    let thread = setup(&mut c, &root, PermissionMode::ReadOnly).await;

    let run = ask_forge(&mut c, thread, "@Forge look around").await;
    let seen = c.until_ended(run).await;
    assert_eq!(status_of(&seen), RunStatus::Done, "{seen:#?}");
    let events = events_of(&seen, run);
    let sessions: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SessionStarted { .. }))
        .collect();
    assert_eq!(
        sessions,
        [&Event::SessionStarted {
            session_id: TID.into(),
            model: "gpt-test".into(),
            cwd: "/work/demo".into(),
        }],
        "once, though the fake also sends thread/started"
    );
    assert!(events.iter().any(|e| matches!(e,
        Event::ToolCall { id, name, .. } if id == "exec-1" && name == "Bash")));
    assert!(events.iter().any(|e| matches!(e,
        Event::ToolResult { id, is_error: false, output } if id == "exec-1" && output == "ok\n")));
    assert!(
        matches!(events.last(), Some(Event::TurnEnd { ok: true, summary, .. }) if summary == "All done.")
    );
    assert_eq!(agent_texts(&seen), ["All done."]);

    // The thread id is the run's session, for the next run to resume.
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs[0].session_id, TID);

    // Read-only: no worktree, a read-only sandbox, approval on request.
    let msgs = messages(dir, 1);
    let start = &find(&msgs, "thread/start")["params"];
    let root = std::fs::canonicalize(&root).unwrap();
    assert_eq!(
        std::fs::canonicalize(start["cwd"].as_str().unwrap()).unwrap(),
        root
    );
    assert_eq!(start["model"], "gpt-test");
    assert_eq!(start["approvalPolicy"], "on-request");
    assert_eq!(start["sandbox"], "read-only");
    let instructions = start["developerInstructions"].as_str().unwrap();
    assert!(
        instructions.starts_with(
            "You are Forge, Builds things, working in Crew with other agents in project demo.\n\
             You are read-only: review and report, don't change files."
        ),
        "{instructions}"
    );
    assert!(instructions.ends_with("\n\nBe brief."), "{instructions}");
    assert!(
        !instructions.contains("handoff"),
        "not wired: {instructions}"
    );
    let turn = &find(&msgs, "turn/start")["params"];
    assert_eq!(
        turn["input"],
        json!([{"type": "text", "text": "@Forge look around", "text_elements": []}])
    );
    // No crew-mcp, so no MCP server for it.
    let args = args_log(dir);
    assert!(args.starts_with("login status\n"), "{args}");
    assert!(args.contains("\napp-server\n"), "{args}");
}

#[tokio::test(flavor = "multi_thread")]
async fn command_approvals_are_decided_in_crew() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut steps = start();
    steps.extend([
        item("item/started", command("exec-1", "mkdir sub", "inProgress")),
        json!({"request": "item/commandExecution/requestApproval", "id": 0, "params": approval_params("exec-1", "mkdir sub"), "reply": {"decision": "accept"}}),
        item("item/completed", command("exec-1", "mkdir sub", "completed")),
        item("item/started", command("exec-2", "rm -rf sub", "inProgress")),
        json!({"request": "item/commandExecution/requestApproval", "id": 1, "params": approval_params("exec-2", "rm -rf sub"), "reply": {"decision": "decline"}}),
    ]);
    steps.extend(finish("Made sub."));
    scenario(dir, 1, steps);
    let d = Daemon::start(codex_script(dir, 0), None).await;
    let mut c = d.client().await;
    let thread = setup(&mut c, &d.tmp.path().join("repo"), PermissionMode::ReadOnly).await;
    let run = ask_forge(&mut c, thread, "@Forge make a folder").await;

    let asked = c.until_asked().await;
    assert_eq!(asked.run, run);
    assert_eq!(
        asked.kind,
        ApprovalKind::Permission {
            tool: "Bash".into(),
            input: json!({"command": "mkdir sub", "cwd": "/work/demo"}),
            tool_use_id: Some("exec-1".into()),
        }
    );
    c.decide(asked.id, true).await;
    let second = c.until_asked().await;
    assert_ne!(second.id, asked.id);
    c.decide(second.id, false).await;

    let seen = c.until_ended(run).await;
    assert_eq!(status_of(&seen), RunStatus::Done, "{seen:#?}");
    assert!(
        system_texts(&seen).iter().all(|t| !t.contains("failed")),
        "{seen:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_agents_write_in_the_worktree_and_file_changes_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let change = json!({"type": "fileChange", "id": "exec-9", "status": "inProgress", "changes": [{"path": "/work/demo/notes.md", "kind": {"type": "add"}, "diff": "crew\n"}]});
    let mut steps = start();
    steps.extend([
        item("item/started", change.clone()),
        json!({"request": "item/fileChange/requestApproval", "id": 0, "params": {"threadId": TID, "turnId": "turn-1", "itemId": "exec-9"}, "reply": {"decision": "accept"}}),
    ]);
    steps.extend(finish("Wrote notes."));
    scenario(dir, 1, steps);
    // Any file will do: the fake never starts it.
    let crew_mcp = fake_app_server();
    let d = Daemon::start(codex_script(dir, 0), Some(crew_mcp.clone())).await;
    let mut c = d.client().await;
    let thread = setup(
        &mut c,
        &d.tmp.path().join("repo"),
        PermissionMode::EditFiles,
    )
    .await;
    let run = ask_forge(&mut c, thread, "@Forge write notes").await;

    let asked = c.until_asked().await;
    let ApprovalKind::Permission { tool, input, .. } = &asked.kind else {
        panic!("{asked:?}")
    };
    assert_eq!(tool, "Edit");
    assert_eq!(input["path"], "/work/demo/notes.md");
    assert_eq!(
        input["changes"],
        json!([{"path": "/work/demo/notes.md", "diff": "crew\n"}])
    );
    c.decide(asked.id, true).await;
    let seen = c.until_ended(run).await;
    assert_eq!(status_of(&seen), RunStatus::Done, "{seen:#?}");

    // The worktree is the sandbox's one writable root.
    let msgs = messages(dir, 1);
    let start = &find(&msgs, "thread/start")["params"];
    let cwd = start["cwd"].as_str().unwrap();
    let home = std::fs::canonicalize(&d.home).unwrap();
    assert!(Path::new(cwd).starts_with(home.join("worktrees")), "{cwd}");
    assert_eq!(start["sandbox"], "workspace-write");
    assert_eq!(start["approvalPolicy"], "on-request");
    assert_eq!(
        start["config"]["sandbox_workspace_write"]["writable_roots"],
        json!([cwd])
    );
    let instructions = start["developerInstructions"].as_str().unwrap();
    assert!(
        instructions.contains("You can edit files in this thread's worktree."),
        "{instructions}"
    );
    assert!(
        instructions.contains("call the handoff tool of the crew MCP server"),
        "{instructions}"
    );

    // crew-mcp is passed for this process only, on the command line.
    let args = args_log(dir);
    let line = args.lines().find(|l| l.starts_with("app-server")).unwrap();
    let mcp = std::fs::canonicalize(&crew_mcp).unwrap();
    assert!(
        line.contains(&format!(
            "-c mcp_servers.crew.command=\"{}\"",
            mcp.display()
        )),
        "{line}"
    );
    assert!(
        line.contains(&format!("-c mcp_servers.crew.args=[\"--run\",\"{run}\"]")),
        "{line}"
    );
    assert!(
        line.contains(&format!(
            "-c mcp_servers.crew.env.CREW_HOME=\"{}\"",
            home.display()
        )),
        "{line}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn crew_elicitations_pass_and_other_servers_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let elicit = |server: &str, tool: &str| {
        json!({"threadId": TID, "turnId": "turn-1", "serverName": server, "mode": "form",
               "_meta": {"codex_approval_kind": "mcp_tool_call", "tool_params": {"title": "x"}},
               "message": format!("Allow the {server} MCP server to run tool \"{tool}\"?"),
               "requestedSchema": {"type": "object", "properties": {}}})
    };
    let mut steps = start();
    steps.extend([
        json!({"request": "mcpServer/elicitation/request", "id": 0, "params": elicit("crew", "handoff"), "reply": {"action": "accept", "content": {}, "_meta": null}}),
        json!({"request": "mcpServer/elicitation/request", "id": 1, "params": elicit("github", "create_issue"), "reply": {"action": "decline", "content": null, "_meta": null}}),
    ]);
    steps.extend(finish("Skipped the issue."));
    scenario(dir, 1, steps);
    let d = Daemon::start(codex_script(dir, 0), None).await;
    let mut c = d.client().await;
    let thread = setup(&mut c, &d.tmp.path().join("repo"), PermissionMode::ReadOnly).await;
    let run = ask_forge(&mut c, thread, "@Forge file it").await;

    let asked = c.until_asked().await;
    assert_eq!(
        asked.kind,
        ApprovalKind::Permission {
            tool: "mcp__github__create_issue".into(),
            input: json!({"title": "x"}),
            tool_use_id: None,
        }
    );
    c.decide(asked.id, false).await;
    let seen = c.until_ended(run).await;
    assert_eq!(status_of(&seen), RunStatus::Done, "{seen:#?}");

    assert!(
        system_texts(&seen)
            .contains(&r#"Forge wants to use github/create_issue: {"title":"x"}"#.to_string()),
        "{seen:#?}"
    );

    // The crew server's ask never reached you: one approval in all.
    let mut other = d.client().await;
    other.call(Request::Subscribe { thread }).await.unwrap();
    let backlog = other
        .until(|m| matches!(m, ServerMsg::Message { message, .. } if message.body == "Skipped the issue."))
        .await;
    let asks: Vec<ApprovalId> = backlog
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Approval { approval, .. } => Some(approval.id),
            _ => None,
        })
        .collect();
    assert_eq!(asks, [asked.id]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_run_resumes_the_codex_thread() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut first = start();
    first.extend(finish("First."));
    scenario(dir, 1, first);
    let mut second = opening("thread/resume", json!({"threadId": TID}));
    second.extend(finish("Second."));
    scenario(dir, 2, second);
    let d = Daemon::start(codex_script(dir, 0), None).await;
    let mut c = d.client().await;
    let thread = setup(&mut c, &d.tmp.path().join("repo"), PermissionMode::ReadOnly).await;

    let run1 = ask_forge(&mut c, thread, "@Forge one").await;
    let seen = c.until_ended(run1).await;
    assert_eq!(status_of(&seen), RunStatus::Done, "{seen:#?}");
    c.call(Request::PostMessage {
        thread,
        body: "a note for later".into(),
    })
    .await
    .unwrap();
    let run2 = ask_forge(&mut c, thread, "@Forge two").await;
    let seen = c.until_ended(run2).await;
    assert_eq!(status_of(&seen), RunStatus::Done, "{seen:#?}");
    let sessions = events_of(&seen, run2)
        .into_iter()
        .filter(|e| matches!(e, Event::SessionStarted { .. }))
        .count();
    assert_eq!(sessions, 1);

    let msgs = messages(dir, 2);
    assert!(msgs.iter().all(|m| m["method"] != "thread/start"));
    let resume = &find(&msgs, "thread/resume")["params"];
    assert_eq!(resume["threadId"], TID);
    // Only what Forge missed, then the new prompt.
    let turn = &find(&msgs, "turn/start")["params"];
    assert_eq!(turn["threadId"], TID);
    assert_eq!(
        turn["input"][0]["text"],
        "You: a note for later\n\n@Forge two"
    );
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert!(runs.iter().all(|r| r.session_id == TID), "{runs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_interrupts_then_kills_a_codex_that_ignores_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut steps = start();
    steps.extend([
        item("item/started", command("exec-1", "sleep 100", "inProgress")),
        json!({"expect": "turn/interrupt", "match": {"threadId": TID, "turnId": "turn-1"}}),
        json!({"sleep": 120}),
    ]);
    scenario_with(dir, 1, steps, json!({"ignore_sigint": true}));
    let d = Daemon::start(codex_script(dir, 0), None).await;
    let mut c = d.client().await;
    let thread = setup(&mut c, &d.tmp.path().join("repo"), PermissionMode::ReadOnly).await;
    let run = ask_forge(&mut c, thread, "@Forge wait").await;
    c.until(|m| {
        matches!(
            m,
            ServerMsg::Event {
                event: Event::ToolCall { .. },
                ..
            }
        )
    })
    .await;

    let stopped_at = Instant::now();
    c.call(Request::StopRun { run }).await.unwrap();
    let seen = c.until_ended(run).await;
    let took = stopped_at.elapsed();
    assert_eq!(status_of(&seen), RunStatus::Stopped, "{seen:#?}");
    // 3 s for the interrupt, 3 s more for SIGINT, then SIGTERM ends it.
    assert!(took >= Duration::from_secs(5), "stopped after {took:?}");
    assert!(took < WAIT, "stopped after {took:?}");
    assert!(events_of(&seen, run).iter().any(|e| matches!(e,
        Event::ToolResult { id, is_error: true, output } if id == "exec-1" && output == "Stopped.")));
    assert!(system_texts(&seen).contains(&"Forge stopped.".to_string()));
    assert!(
        messages(dir, 1)
            .iter()
            .any(|m| m["method"] == "turn/interrupt")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_mid_turn_fails_the_run_and_cancels_its_asks() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let go = dir.join("go");
    let mut steps = start();
    steps.extend([
        item("item/started", command("exec-1", "make", "inProgress")),
        json!({"send": {"method": "item/commandExecution/requestApproval", "id": 0, "params": approval_params("exec-1", "make")}}),
        json!({"wait_file": go}),
        json!({"stderr": "ERROR codex_core: model connection lost"}),
        json!({"exit": 2}),
    ]);
    scenario(dir, 1, steps);
    let d = Daemon::start(codex_script(dir, 0), None).await;
    let mut c = d.client().await;
    let thread = setup(&mut c, &d.tmp.path().join("repo"), PermissionMode::ReadOnly).await;
    let run = ask_forge(&mut c, thread, "@Forge build").await;
    let asked = c.until_asked().await;
    std::fs::write(&go, "").unwrap();

    let seen = c.until_ended(run).await;
    assert_eq!(status_of(&seen), RunStatus::Failed, "{seen:#?}");
    let texts = system_texts(&seen);
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("Forge failed:") && t.contains("model connection lost")),
        "{texts:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, ServerMsg::Approval { approval, .. }
        if approval.id == asked.id && approval.status == ApprovalStatus::Cancelled))
    );
    assert!(events_of(&seen, run).iter().any(|e| matches!(e,
        Event::ToolResult { id, is_error: true, .. } if id == "exec-1")));
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_out_codex_fails_fast() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let d = Daemon::start(codex_script(dir, 1), None).await;
    let mut c = d.client().await;
    let thread = setup(&mut c, &d.tmp.path().join("repo"), PermissionMode::ReadOnly).await;
    let run = ask_forge(&mut c, thread, "@Forge hi").await;
    let seen = c.until_ended(run).await;
    assert_eq!(status_of(&seen), RunStatus::Failed);
    assert!(
        system_texts(&seen).contains(&SIGNED_OUT.to_string()),
        "{seen:#?}"
    );
    assert!(!args_log(dir).contains("app-server"));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_codex_fails_fast() {
    let d = Daemon::start(PathBuf::from("/nonexistent/codex"), None).await;
    let mut c = d.client().await;
    let thread = setup(&mut c, &d.tmp.path().join("repo"), PermissionMode::ReadOnly).await;
    let run = ask_forge(&mut c, thread, "@Forge hi").await;
    let seen = c.until_ended(run).await;
    assert_eq!(status_of(&seen), RunStatus::Failed);
    assert!(
        system_texts(&seen).contains(&NOT_INSTALLED.to_string()),
        "{seen:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn runtimes_list_codex_models_and_sign_in() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scenario(
        dir,
        1,
        vec![
            json!({"expect": "initialize", "as": "init"}),
            json!({"respond": "init", "result": {}}),
            json!({"expect": "initialized"}),
            json!({"expect": "model/list", "as": "list"}),
            json!({"respond": "list", "result": {"data": [
                {"id": "gpt-a", "model": "gpt-a", "displayName": "GPT A", "hidden": false, "isDefault": true},
                {"id": "gpt-old", "model": "gpt-old", "displayName": "GPT Old", "hidden": true},
                {"id": "gpt-b", "model": "gpt-b", "displayName": "", "hidden": false}
            ], "nextCursor": null}}),
        ],
    );
    let d = Daemon::start(codex_script(dir, 0), None).await;
    let mut c = d.client().await;
    let list: Vec<RuntimeInfo> = c.ok(Request::Runtimes).await;
    let codex = list.iter().find(|r| r.runtime == Runtime::Codex).unwrap();
    assert!(codex.installed);
    assert_eq!(codex.version.as_deref(), Some("0.165.0"));
    assert_eq!(codex.signed_in, Some(true));
    let models: Vec<(&str, &str)> = codex
        .models
        .iter()
        .map(|m| (m.id.as_str(), m.label.as_str()))
        .collect();
    assert_eq!(models, [("gpt-a", "GPT A"), ("gpt-b", "gpt-b")]);
    let claude = list
        .iter()
        .find(|r| r.runtime == Runtime::ClaudeCode)
        .unwrap();
    let ids: Vec<&str> = claude.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["opus", "sonnet", "haiku"]);

    // Signed out, the status command says so by its exit code.
    let out = tempfile::tempdir().unwrap();
    let d = Daemon::start(codex_script(out.path(), 1), None).await;
    let mut c = d.client().await;
    let list: Vec<RuntimeInfo> = c.ok(Request::Runtimes).await;
    let codex = list.iter().find(|r| r.runtime == Runtime::Codex).unwrap();
    assert_eq!(codex.signed_in, Some(false));
    assert!(codex.models.is_empty(), "its app-server has no scenario");
}
