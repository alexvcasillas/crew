//! Generated thread titles: a short run after a thread's first message
//! that is never a run of an agent.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ProjectId, ThreadId};
use crew_core::store::{RuntimeUsage, StoredRun, StoredThread};
use crew_proto::{
    ClientMsg, PROTO_VERSION, Request, ServerMsg, TitleMode, TitleModel, TitleSettings,
    TitleSource, read_frame, write_frame,
};
use crewd::{ServeOptions, serve};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Generous, for a loaded CI runner.
const WAIT: Duration = Duration::from_secs(30);

const FIRST: &str = "@Ada can you look at why the parallel runner flakes on linux sometimes";
const RESULT_LINE: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","duration_ms":5,"num_turns":1,"permission_denials":[],"session_id":"00000000-0000-4000-8000-000000000001"}"#;

fn fake_app_server() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crew-core/tests/fake/codex_app_server.py")
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `claude` driven by files in `dir`:
/// - `auth status` exits 0, or with `{"loggedIn": false}` and 1 when
///   `signed-out` exists.
/// - A title run (it has `--no-session-persistence`) logs its arguments
///   to `title-args.log`, one per line, waits for `title-go` while
///   `title-hold` exists, prints `title-reply`, exits with `title-exit`
///   (0 if missing) and then writes `title-done`.
/// - Any other run is an agent's: it appends a line to `runs.log` and
///   answers "done".
fn claude_script(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    let d = dir.display();
    executable(
        &path,
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = auth ]; then\n\
               if [ -f '{d}/signed-out' ]; then echo '{{\"loggedIn\": false}}'; exit 1; fi\n\
               exit 0\n\
             fi\n\
             for a in \"$@\"; do\n\
               if [ \"$a\" = --no-session-persistence ]; then\n\
                 printf '%s\\n' \"$@\" > '{d}/title-args.log'\n\
                 if [ -f '{d}/title-hold' ]; then\n\
                   while [ ! -f '{d}/title-go' ]; do sleep 0.02; done\n\
                 fi\n\
                 cat '{d}/title-reply' 2>/dev/null\n\
                 code=$(cat '{d}/title-exit' 2>/dev/null || echo 0)\n\
                 touch '{d}/title-done'\n\
                 exit $code\n\
               fi\n\
             done\n\
             echo run >> '{d}/runs.log'\n\
             echo '{RESULT_LINE}'\n"
        ),
    );
    path
}

/// A `codex` that is signed in, lists models from `scenario.json` on its
/// app-server and, as `codex exec`, logs its arguments to
/// `codex-args.log` and prints "Codex named it".
fn codex_script(dir: &Path) -> PathBuf {
    let path = dir.join("codex");
    let d = dir.display();
    executable(
        &path,
        &format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             login) exit 0 ;;\n\
             app-server) exec python3 '{fake}' '{d}/scenario.json' ;;\n\
             exec) printf '%s\\n' \"$@\" > '{d}/codex-args.log'; echo 'Codex named it' ;;\n\
             *) exit 2 ;;\n\
             esac\n",
            fake = fake_app_server().display(),
        ),
    );
    let steps = vec![
        json!({"expect": "initialize", "as": "init"}),
        json!({"respond": "init", "result": {}}),
        json!({"expect": "initialized"}),
        json!({"expect": "model/list", "as": "list"}),
        json!({"respond": "list", "result": {"data": [
            {"id": "gpt-big", "model": "gpt-big", "displayName": "GPT Big", "hidden": false},
            {"id": "gpt-luna-mini", "model": "gpt-luna-mini", "displayName": "Luna mini", "hidden": false}
        ], "nextCursor": null}}),
    ];
    std::fs::write(
        dir.join("scenario.json"),
        json!({"steps": steps}).to_string(),
    )
    .unwrap();
    path
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

struct Client {
    stream: UnixStream,
    next_id: u64,
    pending: Vec<ServerMsg>,
}

impl Client {
    async fn connect(home: &Path) -> Client {
        let mut c = Client {
            stream: UnixStream::connect(home.join("crewd.sock")).await.unwrap(),
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

    /// The first message, kept or read now, that matches. Messages that
    /// don't match stay for a later call.
    async fn until<T>(&mut self, mut pick: impl FnMut(&ServerMsg) -> Option<T>) -> T {
        for i in 0..self.pending.len() {
            if let Some(found) = pick(&self.pending[i]) {
                self.pending.remove(i);
                return found;
            }
        }
        loop {
            let msg = self.read().await;
            match pick(&msg) {
                Some(found) => return found,
                None => self.pending.push(msg),
            }
        }
    }

    /// The next copy of `thread` crewd reports.
    async fn thread_change(&mut self, thread: ThreadId) -> StoredThread {
        self.until(|m| match m {
            ServerMsg::ThreadChanged { thread: t } if t.id == thread => Some(t.clone()),
            _ => None,
        })
        .await
    }
}

fn spec(name: &str, mode: PermissionMode) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        role: "Builds things".into(),
        runtime: Runtime::ClaudeCode,
        model: "opus".into(),
        instructions: String::new(),
        permission_mode: mode,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    }
}

struct World {
    _tmp: TempDir,
    dir: PathBuf,
    /// Subscribed to everything.
    app: Client,
    thread: ThreadId,
}

impl World {
    /// crewd with the fakes, a project with a read-only agent Ada and an
    /// editing agent Wes, and a thread titled with `FIRST`'s line.
    async fn start() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let home = dir.join("crew");
        let opts = ServeOptions {
            claude_bin: Some(claude_script(&dir)),
            codex_bin: Some(codex_script(&dir)),
            crew_mcp_bin: None,
            ..Default::default()
        };
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

        let mut app = Client::connect(&home).await;
        app.call(Request::SubscribeAll).await.unwrap();
        let root = dir.join("repo");
        git_repo(&root);
        let project: ProjectId = app
            .ok(Request::CreateProject {
                name: "demo".into(),
                root: root.to_string_lossy().into_owned(),
                main_branch: "main".into(),
                work_mode: None,
                copy_files: vec![],
                setup_command: None,
            })
            .await;
        let channel = app
            .ok(Request::CreateChannel {
                project,
                name: "general".into(),
            })
            .await;
        for (name, mode) in [
            ("Ada", PermissionMode::ReadOnly),
            ("Wes", PermissionMode::EditFiles),
        ] {
            let _: AgentId = app
                .ok(Request::CreateAgent {
                    spec: spec(name, mode),
                })
                .await;
        }
        let thread: ThreadId = app
            .ok(Request::CreateThread {
                channel,
                title: FIRST.into(),
            })
            .await;
        World {
            _tmp: tmp,
            dir,
            app,
            thread,
        }
    }

    fn reply(&self, text: &str) {
        std::fs::write(self.dir.join("title-reply"), text).unwrap();
    }

    async fn post(&mut self, body: &str) {
        self.app
            .call(Request::PostMessage {
                thread: self.thread,
                body: body.into(),
                skill: None,
                model_override: None,
            })
            .await
            .unwrap();
    }

    /// Waits for the agent runs to end, `n` of them in all.
    async fn runs_end(&mut self, n: usize) {
        let mut ended = 0;
        while ended < n {
            self.app
                .until(|m| match m {
                    ServerMsg::RunStatus { status, .. }
                        if !matches!(status, RunStatus::Queued | RunStatus::Working) =>
                    {
                        Some(())
                    }
                    _ => None,
                })
                .await;
            ended += 1;
        }
    }

    async fn stored(&mut self) -> StoredThread {
        let channel = self.stored_channel().await;
        let threads: Vec<StoredThread> = self.app.ok(Request::ListThreads { channel }).await;
        threads.into_iter().find(|t| t.id == self.thread).unwrap()
    }

    async fn stored_channel(&mut self) -> crew_core::ids::ChannelId {
        let projects: Vec<crew_core::store::StoredProject> =
            self.app.ok(Request::ListProjects).await;
        let channels: Vec<crew_core::store::StoredChannel> = self
            .app
            .ok(Request::ListChannels {
                project: projects[0].id,
            })
            .await;
        channels[0].id
    }

    fn title_args(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("title-args.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Waits until the fake title run has finished.
    async fn title_done(&self) {
        let done = self.dir.join("title-done");
        timeout(WAIT, async {
            while !done.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the title run never ended");
    }
}

#[tokio::test]
async fn a_generated_title_replaces_the_first_line_and_is_never_a_run() {
    let mut w = World::start().await;
    w.reply(
        r#"{"type":"result","subtype":"success","is_error":false,"result":"  \"Flaky parallel runner on Linux.\"\nmore"}"#,
    );
    w.post(FIRST).await;

    let generating = w.app.thread_change(w.thread).await;
    assert_eq!(generating.title_source, TitleSource::Generating);
    assert_eq!(generating.title, FIRST);
    let titled = w.app.thread_change(w.thread).await;
    assert_eq!(titled.title, "Flaky parallel runner on Linux");
    assert_eq!(titled.title_source, TitleSource::Generated);

    // Haiku, no tools, no session, no slash commands or MCP servers, the
    // prompt last after `--`.
    let args = w.title_args();
    let at = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("no {flag} in {args:?}"))
    };
    assert_eq!(args[0], "-p");
    assert_eq!(args[at("--model") + 1], "haiku");
    assert_eq!(args[at("--tools") + 1], "");
    assert_eq!(args[at("--output-format") + 1], "json");
    at("--disable-slash-commands");
    at("--strict-mcp-config");
    let dashes = at("--");
    assert!(
        args[dashes + 1..].join("\n").contains(FIRST),
        "the prompt holds the message: {args:?}"
    );

    // Only Ada's run is a run: in the list, the history and the usage.
    w.runs_end(1).await;
    let runs: Vec<StoredRun> = w.app.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 1);
    let history: Vec<StoredRun> = w
        .app
        .ok(Request::RunHistory {
            before: None,
            limit: 50,
        })
        .await;
    assert_eq!(history.len(), 1);
    let usage: Vec<RuntimeUsage> = w.app.ok(Request::UsageSince { since_ms: 0 }).await;
    assert_eq!(usage.iter().map(|u| u.runs).sum::<u32>(), 1);
    let ran = std::fs::read_to_string(w.dir.join("runs.log")).unwrap();
    assert_eq!(ran.lines().count(), 1);

    // A later message names nothing.
    std::fs::remove_file(w.dir.join("title-args.log")).unwrap();
    w.post("@Ada and on macOS?").await;
    w.runs_end(1).await;
    assert!(w.title_args().is_empty());
    assert_eq!(w.stored().await.title, "Flaky parallel runner on Linux");
}

#[tokio::test]
async fn a_rename_while_the_title_is_generating_wins() {
    let mut w = World::start().await;
    w.reply(r#"{"type":"result","is_error":false,"result":"Their title"}"#);
    std::fs::write(w.dir.join("title-hold"), "").unwrap();
    w.post(FIRST).await;
    assert_eq!(
        w.app.thread_change(w.thread).await.title_source,
        TitleSource::Generating
    );

    w.app
        .call(Request::RenameThread {
            thread: w.thread,
            title: "Mine".into(),
        })
        .await
        .unwrap();
    let renamed = w.app.thread_change(w.thread).await;
    assert_eq!(
        (renamed.title.as_str(), renamed.title_source),
        ("Mine", TitleSource::User)
    );

    std::fs::write(w.dir.join("title-go"), "").unwrap();
    w.title_done().await;
    // Time for crewd to take the answer in.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stored = w.stored().await;
    assert_eq!(
        (stored.title.as_str(), stored.title_source),
        ("Mine", TitleSource::User)
    );
}

#[tokio::test]
async fn a_failed_title_run_keeps_the_first_line() {
    let mut w = World::start().await;
    w.reply(r#"{"type":"result","is_error":false,"result":"Never used"}"#);
    std::fs::write(w.dir.join("title-exit"), "1").unwrap();
    w.post(FIRST).await;
    assert_eq!(
        w.app.thread_change(w.thread).await.title_source,
        TitleSource::Generating
    );
    let kept = w.app.thread_change(w.thread).await;
    assert_eq!(
        (kept.title.as_str(), kept.title_source),
        (FIRST, TitleSource::FirstLine)
    );
}

#[tokio::test]
async fn an_answer_with_nothing_in_it_keeps_the_first_line() {
    let mut w = World::start().await;
    w.reply(r#"{"type":"result","is_error":false,"result":"  \"\"  "}"#);
    w.post(FIRST).await;
    w.app.thread_change(w.thread).await;
    let kept = w.app.thread_change(w.thread).await;
    assert_eq!(
        (kept.title.as_str(), kept.title_source),
        (FIRST, TitleSource::FirstLine)
    );
}

#[tokio::test]
async fn without_claude_signed_in_codex_names_the_thread_on_its_cheapest_model() {
    let mut w = World::start().await;
    std::fs::write(w.dir.join("signed-out"), "").unwrap();
    w.post(FIRST).await;
    w.app.thread_change(w.thread).await;
    let titled = w.app.thread_change(w.thread).await;
    assert_eq!(
        (titled.title.as_str(), titled.title_source),
        ("Codex named it", TitleSource::Generated)
    );
    assert!(w.title_args().is_empty(), "claude ran no title");
    let args: Vec<String> = std::fs::read_to_string(w.dir.join("codex-args.log"))
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(args[0], "exec");
    let at = |flag: &str| args.iter().position(|a| a == flag).unwrap();
    assert_eq!(args[at("--model") + 1], "gpt-luna-mini");
    assert_eq!(args[at("--sandbox") + 1], "read-only");
    at("--ephemeral");
    assert!(args[at("--") + 1..].join("\n").contains(FIRST));
}

#[tokio::test]
async fn the_settings_pick_the_model_or_keep_first_lines() {
    let mut w = World::start().await;
    // Sonnet.
    let sonnet = TitleSettings {
        mode: TitleMode::Generated,
        model: TitleModel::Sonnet,
    };
    w.app
        .call(Request::SetTitleSettings { titles: sonnet })
        .await
        .unwrap();
    let told = w
        .app
        .until(|m| match m {
            ServerMsg::TitleSettingsChanged { titles } => Some(*titles),
            _ => None,
        })
        .await;
    assert_eq!(told, sonnet);
    w.reply(r#"{"type":"result","is_error":false,"result":"On Sonnet"}"#);
    w.post(FIRST).await;
    w.app.thread_change(w.thread).await;
    assert_eq!(w.app.thread_change(w.thread).await.title, "On Sonnet");
    let args = w.title_args();
    let model = args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(args[model + 1], "sonnet");

    // First line: no title run at all, and a new client hears it first.
    let first_line = TitleSettings {
        mode: TitleMode::FirstLine,
        model: TitleModel::Haiku,
    };
    w.app
        .call(Request::SetTitleSettings { titles: first_line })
        .await
        .unwrap();
    std::fs::remove_file(w.dir.join("title-args.log")).unwrap();
    let channel = w.stored_channel().await;
    let thread: ThreadId = w
        .app
        .ok(Request::CreateThread {
            channel,
            title: "keep this line".into(),
        })
        .await;
    w.thread = thread;
    w.post("@Ada keep this line").await;
    w.runs_end(2).await;
    assert!(w.title_args().is_empty());
    assert_eq!(w.stored().await.title_source, TitleSource::FirstLine);
}

#[tokio::test]
async fn the_branch_keeps_the_first_line_once_a_title_is_generated() {
    let mut w = World::start().await;
    w.reply(r#"{"type":"result","is_error":false,"result":"Flaky runner"}"#);
    // Ada reads only, so the thread has no worktree yet.
    w.post(FIRST).await;
    w.app.thread_change(w.thread).await;
    assert_eq!(w.app.thread_change(w.thread).await.title, "Flaky runner");
    w.runs_end(1).await;

    // Wes edits: his run makes the worktree now.
    w.post("@Wes fix it").await;
    let branch = w
        .app
        .until(|m| match m {
            ServerMsg::ThreadChanged { thread }
                if thread.id == w.thread && thread.worktree.is_some() =>
            {
                Some(thread.worktree.clone().unwrap().branch)
            }
            _ => None,
        })
        .await;
    let slug = crew_core::slug::slugify(FIRST, &|_| false);
    assert_eq!(branch, format!("crew/{slug}"));
    assert_eq!(w.stored().await.title, "Flaky runner");
}
