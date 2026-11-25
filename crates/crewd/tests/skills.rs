//! Skills: discovery for both CLIs and running a message with one.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ProjectId, RunId, ThreadId};
use crew_core::store::{StoredAgent, StoredRun};
use crew_proto::{
    ClientMsg, PROTO_VERSION, Request, ServerMsg, SkillInfo, SkillRef, SkillSource, read_frame,
    write_frame,
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

/// A `claude` that logs each argument on its own line to `args.log`, prints
/// an init line that lists skills, then answers.
fn claude_script(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    let d = dir.display();
    let body = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" > '{d}/args.log'\n\
         echo '{{\"type\":\"system\",\"subtype\":\"init\",\"cwd\":\"/work/demo\",\"session_id\":\"00000000-0000-4000-8000-000000000001\",\"model\":\"claude-test\",\"tools\":[],\"skills\":[\"lint\"],\"slash_commands\":[\"lint\",\"tools:deploy\"]}}'\n\
         echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"duration_ms\":5,\"num_turns\":1,\"permission_denials\":[],\"session_id\":\"00000000-0000-4000-8000-000000000001\"}}'\n"
    );
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The arguments the fake claude was started with.
fn claude_args(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("args.log"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

struct Daemon {
    tmp: TempDir,
    home: PathBuf,
}

impl Daemon {
    /// `claude_home` stands in for `~/.claude`.
    async fn start(claude_bin: PathBuf, codex_bin: PathBuf, claude_home: PathBuf) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        let path = home.clone();
        tokio::spawn(async move {
            let opts = ServeOptions {
                no_title_runs: true,
                claude_bin: Some(claude_bin),
                codex_bin: Some(codex_bin),
                crew_mcp_bin: None,
                runtimes_wait: Some(WAIT),
                claude_home: Some(claude_home),
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

fn spec(name: &str, runtime: Runtime, model: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        role: "Builds things".into(),
        runtime,
        model: model.into(),
        instructions: String::new(),
        permission_mode: PermissionMode::ReadOnly,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

struct World {
    _d: Daemon,
    _scratch: TempDir,
    c: Client,
    dir: PathBuf,
    claude_home: PathBuf,
    root: PathBuf,
    project: ProjectId,
    thread: ThreadId,
}

/// A project with a Claude agent "Ada" (haiku) and a Codex agent "Cody",
/// and a thread. `login_exit` is what `codex login status` exits with.
async fn world(login_exit: i32) -> World {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().to_path_buf();
    let claude_home = dir.join("claude-home");
    let d = Daemon::start(
        claude_script(&dir),
        codex_script(&dir, login_exit),
        claude_home.clone(),
    )
    .await;
    let mut c = d.client().await;
    let root = d.tmp.path().join("repo");
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
    let channel = c
        .ok(Request::CreateChannel {
            project,
            name: "general".into(),
        })
        .await;
    let _: AgentId = c
        .ok(Request::CreateAgent {
            spec: spec("Ada", Runtime::ClaudeCode, "haiku"),
        })
        .await;
    let _: AgentId = c
        .ok(Request::CreateAgent {
            spec: spec("Cody", Runtime::Codex, "gpt-test"),
        })
        .await;
    let thread: ThreadId = c
        .ok(Request::CreateThread {
            channel,
            title: "work".into(),
            first_line: false,
        })
        .await;
    c.call(Request::Subscribe { thread }).await.unwrap();
    World {
        _d: d,
        _scratch: scratch,
        c,
        dir,
        claude_home,
        root,
        project,
        thread,
    }
}

fn names(list: &[SkillInfo]) -> Vec<(Runtime, &str, SkillSource, bool)> {
    list.iter()
        .map(|s| {
            (
                s.runtime.clone(),
                s.name.as_str(),
                s.source.clone(),
                s.shown,
            )
        })
        .collect()
}

fn codex_list_steps(skills: Value) -> Vec<Value> {
    vec![
        json!({"expect": "initialize", "as": "init"}),
        json!({"respond": "init", "result": {}}),
        json!({"expect": "initialized"}),
        json!({"expect": "skills/list", "as": "list"}),
        json!({"respond": "list", "result": {"data": [
            {"cwd": "/work/demo", "errors": [], "skills": skills}
        ]}}),
    ]
}

async fn post(w: &mut World, body: &str, skill: Option<SkillRef>, model: Option<&str>) -> RunId {
    w.c.call(Request::PostMessage {
        thread: w.thread,
        body: body.into(),
        skill,
        model_override: model.map(String::from),
    })
    .await
    .unwrap();
    let seen =
        w.c.until(|m| {
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

#[tokio::test(flavor = "multi_thread")]
async fn lists_both_runtimes_with_sources_and_the_show_toggle() {
    let mut w = world(0).await;
    write(
        &w.claude_home.join("skills/unslop/SKILL.md"),
        "---\nname: unslop\ndescription: Cuts slop\n---\n",
    );
    write(
        &w.claude_home.join("skills/review/SKILL.md"),
        "---\nname: review\n---\n",
    );
    write(
        &w.root.join(".claude/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Project review\n---\n",
    );
    write(
        &w.claude_home.join("skills/broken/SKILL.md"),
        "---\nname: broken\n",
    );
    scenario(
        &w.dir,
        1,
        codex_list_steps(json!([
            {"name": "review", "description": "Codex review", "path": "/s/review/SKILL.md",
             "enabled": true, "scope": "user"},
            {"name": "unslop", "description": "Codex unslop", "path": "/s/unslop/SKILL.md",
             "enabled": true, "scope": "repo"}
        ])),
    );
    let list: Vec<SkillInfo> =
        w.c.ok(Request::Skills {
            project: w.project,
            force_reload: false,
        })
        .await;
    let claude = Runtime::ClaudeCode;
    // A name in both runtimes shows once per runtime; project wins in each.
    assert_eq!(
        names(&list),
        [
            (claude.clone(), "review", SkillSource::Project, true),
            (claude.clone(), "broken", SkillSource::Global, true),
            (claude.clone(), "unslop", SkillSource::Global, true),
            (Runtime::Codex, "review", SkillSource::Global, true),
            (Runtime::Codex, "unslop", SkillSource::Project, true),
        ]
    );
    assert_eq!(list[0].description, "Project review");
    assert_eq!(list[1].description, "");
    let asked = messages(&w.dir, 1);
    let params = &find(&asked, "skills/list")["params"];
    assert_eq!(params["cwds"], json!([w.root.to_string_lossy()]));
    assert_eq!(params["forceReload"], json!(false));

    // Hidden skills are still returned.
    let _: Value =
        w.c.ok(Request::SetSkillShown {
            runtime: claude.clone(),
            source: SkillSource::Global,
            name: "unslop".into(),
            shown: false,
            project: None,
        })
        .await;
    // A project's own skill is hidden in one project, named.
    let refused =
        w.c.call(Request::SetSkillShown {
            runtime: claude.clone(),
            source: SkillSource::Project,
            name: "review".into(),
            shown: false,
            project: None,
        })
        .await;
    assert_eq!(refused, Err("Say which project the skill is in.".into()));
    let list: Vec<SkillInfo> =
        w.c.ok(Request::Skills {
            project: w.project,
            force_reload: false,
        })
        .await;
    let hidden: Vec<(Runtime, &str, bool)> = list
        .iter()
        .map(|s| (s.runtime.clone(), s.name.as_str(), s.shown))
        .filter(|(_, _, shown)| !shown)
        .collect();
    assert_eq!(hidden, [(claude.clone(), "unslop", false)]);
    // Kept: no second app-server for that.
    assert_eq!(args_log(&w.dir).matches("app-server").count(), 1);

    // A new skill shows up only on reload, and Codex is asked to reload.
    write(
        &w.root.join(".claude/skills/fresh/SKILL.md"),
        "---\nname: fresh\n---\n",
    );
    scenario(&w.dir, 2, codex_list_steps(json!([])));
    let list: Vec<SkillInfo> =
        w.c.ok(Request::Skills {
            project: w.project,
            force_reload: false,
        })
        .await;
    assert!(!list.iter().any(|s| s.name == "fresh"));
    let list: Vec<SkillInfo> =
        w.c.ok(Request::Skills {
            project: w.project,
            force_reload: true,
        })
        .await;
    assert!(list.iter().any(|s| s.name == "fresh"));
    assert!(list.iter().all(|s| s.runtime == claude));
    let asked = messages(&w.dir, 2);
    assert_eq!(
        find(&asked, "skills/list")["params"]["forceReload"],
        json!(true)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_skills_are_left_out_when_signed_out() {
    let mut w = world(1).await;
    write(
        &w.claude_home.join("skills/unslop/SKILL.md"),
        "---\nname: unslop\n---\n",
    );
    let list: Vec<SkillInfo> =
        w.c.ok(Request::Skills {
            project: w.project,
            force_reload: false,
        })
        .await;
    assert_eq!(
        names(&list),
        [(Runtime::ClaudeCode, "unslop", SkillSource::Global, true)]
    );
    assert!(!args_log(&w.dir).contains("app-server"));
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_missing_leaves_claude_skills() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path();
    let home = dir.join("claude-home");
    write(
        &home.join("skills/unslop/SKILL.md"),
        "---\nname: unslop\n---\n",
    );
    let d = Daemon::start(
        claude_script(dir),
        PathBuf::from("/nonexistent/codex"),
        home,
    )
    .await;
    let mut c = d.client().await;
    let root = d.tmp.path().join("repo");
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
    let list: Vec<SkillInfo> = c
        .ok(Request::Skills {
            project,
            force_reload: false,
        })
        .await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].runtime, Runtime::ClaudeCode);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claude_skill_run_starts_with_the_skill_and_the_override_model() {
    let mut w = world(0).await;
    let skill = SkillRef {
        runtime: Runtime::ClaudeCode,
        name: "lint".into(),
        path: "/work/demo/.claude/skills/lint/SKILL.md".into(),
        agent: None,
    };
    let run = post(&mut w, "@Ada the whole crate", Some(skill), Some("opus")).await;
    let seen = w.c.until_ended(run).await;
    let details = seen
        .iter()
        .find_map(|m| match m {
            ServerMsg::RunStatus { details, .. } => Some(details.clone()),
            _ => None,
        })
        .unwrap();
    // The run card knows the skill from the first status on.
    assert_eq!(details.skill.as_deref(), Some("lint"));
    let args = claude_args(&w.dir);
    let at = args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(args[at + 1], "opus");
    assert_eq!(args[args.len() - 2], "--");
    assert!(
        args.last()
            .unwrap()
            .starts_with("/lint @Ada the whole crate"),
        "{args:?}"
    );
    // The agent keeps its own model.
    let agents: Vec<StoredAgent> =
        w.c.ok(Request::ListAgents {
            include_deleted: false,
        })
        .await;
    let ada = agents.iter().find(|a| a.spec.name == "Ada").unwrap();
    assert_eq!(ada.spec.model, "haiku");
    let runs: Vec<StoredRun> = w.c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs[0].skill.as_deref(), Some("lint"));

    // The next message, with no skill and no override, is plain.
    let run = post(&mut w, "@Ada hello", None, None).await;
    w.c.until_ended(run).await;
    let args = claude_args(&w.dir);
    let at = args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(args[at + 1], "haiku");
    assert!(!args.last().unwrap().starts_with('/'));
    let runs: Vec<StoredRun> = w.c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs[1].skill, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_skill_run_sends_a_skill_input_and_the_override_model() {
    let mut w = world(0).await;
    let mut steps = start();
    steps.extend(finish("Linted."));
    scenario(&w.dir, 1, steps);
    let skill = SkillRef {
        runtime: Runtime::Codex,
        name: "lint".into(),
        path: "/work/demo/.codex/skills/lint/SKILL.md".into(),
        agent: None,
    };
    let run = post(&mut w, "@Cody everything", Some(skill), Some("gpt-big")).await;
    w.c.until_ended(run).await;
    let asked = messages(&w.dir, 1);
    assert_eq!(find(&asked, "thread/start")["params"]["model"], "gpt-big");
    let input = &find(&asked, "turn/start")["params"]["input"];
    assert_eq!(input[0]["type"], "text");
    assert!(
        input[0]["text"]
            .as_str()
            .unwrap()
            .contains("@Cody everything")
    );
    assert_eq!(
        input[1],
        json!({"type": "skill", "name": "lint", "path": "/work/demo/.codex/skills/lint/SKILL.md"})
    );
    let agents: Vec<StoredAgent> =
        w.c.ok(Request::ListAgents {
            include_deleted: false,
        })
        .await;
    let cody = agents.iter().find(|a| a.spec.name == "Cody").unwrap();
    assert_eq!(cody.spec.model, "gpt-test");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_needs_an_agent_of_its_runtime() {
    let mut w = world(0).await;
    let skill = SkillRef {
        runtime: Runtime::Codex,
        name: "lint".into(),
        path: "/s/SKILL.md".into(),
        agent: None,
    };
    let wrong =
        w.c.call(Request::PostMessage {
            thread: w.thread,
            body: "@Ada go".into(),
            skill: Some(skill.clone()),
            model_override: None,
        })
        .await
        .unwrap_err();
    assert!(wrong.contains("runs on Codex"), "{wrong}");
    let nobody =
        w.c.call(Request::PostMessage {
            thread: w.thread,
            body: "go".into(),
            skill: Some(skill),
            model_override: None,
        })
        .await
        .unwrap_err();
    assert!(nobody.contains("Mention an agent"), "{nobody}");
    // Nothing was queued or saved.
    let runs: Vec<StoredRun> = w.c.ok(Request::ListRuns { status: None }).await;
    assert!(runs.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_odd_model_override_is_refused_and_nothing_is_saved() {
    let mut w = world(0).await;
    let long = "m".repeat(129);
    for odd in [
        "--dangerously-skip-permissions",
        "-m",
        "two words",
        "bad\nmodel",
        "bell\u{7}",
        long.as_str(),
    ] {
        let err =
            w.c.call(Request::PostMessage {
                thread: w.thread,
                body: "@Ada go".into(),
                skill: None,
                model_override: Some(odd.into()),
            })
            .await
            .unwrap_err();
        assert!(err.contains("isn't valid"), "{odd:?}: {err}");
    }
    let runs: Vec<StoredRun> = w.c.ok(Request::ListRuns { status: None }).await;
    assert!(runs.is_empty());
    // A fresh subscription holds no message and no run.
    w.c.call(Request::Subscribe { thread: w.thread })
        .await
        .unwrap();
    // The next good message is the first the thread has.
    let run = post(&mut w, "@Ada fine", None, Some("opus")).await;
    let runs: Vec<StoredRun> = w.c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run);
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_skills_a_run_did_not_list_are_filtered_out() {
    let mut w = world(1).await;
    let plugin = w.dir.join("plugins-src/tools");
    write(
        &plugin.join("skills/go/SKILL.md"),
        "---\nname: go\ndescription: Goes\n---\n",
    );
    write(
        &plugin.join("skills/stop/SKILL.md"),
        "---\nname: stop\n---\n",
    );
    // A command of the plugin, which the init line lists only under
    // `slash_commands`.
    write(
        &plugin.join("commands/deploy.md"),
        "---\ndescription: Deploy\n---\n",
    );
    write(
        &w.claude_home.join("plugins/installed_plugins.json"),
        &json!({"version": 2, "plugins": {"tools@market": [{"installPath": plugin}]}}).to_string(),
    );
    // Settings are never read, whatever they hold.
    write(&w.claude_home.join("settings.json"), "{{ garbage \u{0}");
    write(
        &w.claude_home.join("skills/unslop/SKILL.md"),
        "---\nname: unslop\n---\n",
    );
    let ask = |project| Request::Skills {
        project,
        force_reload: false,
    };
    let list: Vec<SkillInfo> = w.c.ok(ask(w.project)).await;
    let got: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(got, ["unslop", "tools:go", "tools:stop", "tools:deploy"]);

    // The fake claude's init line lists `lint` only. Once a run has said
    // so, plugin skills it left out are hidden; the others stay.
    let run = post(&mut w, "@Ada hi", None, None).await;
    w.c.until_ended(run).await;
    let list: Vec<SkillInfo> = w.c.ok(ask(w.project)).await;
    let got: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(got, ["unslop", "tools:deploy"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_codex_listing_is_tried_again() {
    let mut w = world(0).await;
    write(
        &w.claude_home.join("skills/unslop/SKILL.md"),
        "---\nname: unslop\n---\n",
    );
    // The first app-server dies before it answers.
    scenario(&w.dir, 1, vec![json!({"exit": 1})]);
    let ask = |project| Request::Skills {
        project,
        force_reload: false,
    };
    let list: Vec<SkillInfo> = w.c.ok(ask(w.project)).await;
    assert_eq!(names(&list).len(), 1);
    scenario(
        &w.dir,
        2,
        codex_list_steps(json!([
            {"name": "review", "description": "", "path": "/s/review/SKILL.md",
             "enabled": true, "scope": "user"}
        ])),
    );
    // No reload asked: the failure wasn't kept.
    let list: Vec<SkillInfo> = w.c.ok(ask(w.project)).await;
    assert_eq!(names(&list).len(), 2);
    assert!(list.iter().any(|s| s.runtime == Runtime::Codex));
    // A good listing is kept.
    let _: Vec<SkillInfo> = w.c.ok(ask(w.project)).await;
    assert_eq!(args_log(&w.dir).matches("app-server").count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_picked_for_one_agent_runs_with_it_alone() {
    let mut w = world(0).await;
    let agents: Vec<StoredAgent> =
        w.c.ok(Request::ListAgents {
            include_deleted: false,
        })
        .await;
    let ada = agents.iter().find(|a| a.spec.name == "Ada").unwrap().id;
    let skill = SkillRef {
        runtime: Runtime::ClaudeCode,
        name: "review".into(),
        path: "/s/SKILL.md".into(),
        agent: Some(ada),
    };
    // Cody is on Codex and stays mentioned; the skill runs with Ada.
    let asked: crew_core::ids::MessageId =
        w.c.ok(Request::PostMessage {
            thread: w.thread,
            body: "@Cody the sync changes".into(),
            skill: Some(skill),
            model_override: Some("opus".into()),
        })
        .await;
    let seen =
        w.c.until(|m| {
            matches!(
                m,
                ServerMsg::RunStatus {
                    status: RunStatus::Queued,
                    ..
                }
            )
        })
        .await;
    let Some(ServerMsg::RunStatus { run, .. }) = seen.last() else {
        unreachable!()
    };
    w.c.until_ended(*run).await;
    let args = claude_args(&w.dir);
    assert!(args.last().unwrap().starts_with("/review "), "{args:?}");
    let runs: Vec<StoredRun> = w.c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 1, "only Ada ran");
    assert_eq!(runs[0].agent, ada);
    assert_eq!(runs[0].skill.as_deref(), Some("review"));
    assert_eq!(runs[0].model_override.as_deref(), Some("opus"));
    assert_eq!(runs[0].message, Some(asked));

    // A plain message keeps who asked and no override.
    let run = post(&mut w, "@Ada hello", None, None).await;
    w.c.until_ended(run).await;
    let runs: Vec<StoredRun> = w.c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs[1].model_override, None);
    assert!(runs[1].message.is_some());
}
