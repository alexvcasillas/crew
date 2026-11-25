//! Projects that aren't one git repo: folders of repos and plain folders,
//! with agents in their own copy per thread or in the folder itself.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::project_kind::{NEEDS_REPO, ProjectKind, WorkMode};
use crew_core::store::{Author, StoredProject, StoredRun, StoredThread};
use crew_proto::{
    ChannelOverview, ClientMsg, FolderInfo, PROTO_VERSION, Request, ServerMsg, SetupFileSize,
    ThreadChanges, read_frame, write_frame,
};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../crew-core/tests/fixtures/claude")
        .join(name)
}

const SESSION: &str = "00000000-0000-4000-8000-000000000001";

/// A fake claude. It logs its folder and arguments to `calls.log`. An
/// agent named Sleeper starts a session and waits to be stopped. Any
/// other agent that may edit writes each `path|content` line of `edits`
/// (relative to its folder) with a Write call, then answers.
fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude.sh");
    let d = dir.display();
    let init = format!(
        r#"{{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"{SESSION}","model":"claude-haiku-4-5","tools":["Write"]}}"#
    );
    let result = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"result":"Done.","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"{SESSION}"}}"#
    );
    let call = format!(
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_%s","name":"Write","input":{{"file_path":"%s/%s","content":"%s"}}}}]}},"session_id":"{SESSION}"}}
"#
    );
    let done = format!(
        r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_%s","content":"ok"}}]}},"session_id":"{SESSION}"}}
"#
    );
    let script = format!(
        r#"#!/bin/sh
D='{d}'
printf 'cwd=%s
' "$PWD" >> "$D/calls.log"
for a in "$@"; do printf 'arg=%s
' "$a" >> "$D/calls.log"; done
printf '%s
' '{init}'
case "$*" in
  *"You are Sleeper"*) sleep 60; exit 0;;
  *"You are read-only"*) printf '%s
' '{result}'; exit 0;;
esac
n=0
if [ -f "$D/edits" ]; then
  while IFS='|' read -r path content; do
    [ -z "$path" ] && continue
    n=$((n+1))
    mkdir -p "$(dirname "$path")"
    printf '%s
' "$content" > "$path"
    printf '{call}' "$n" "$PWD" "$path" "$content"
    printf '{done}' "$n"
  done < "$D/edits"
fi
printf '%s
' '{result}'
"#
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = fixture("read_file.jsonl");
    path
}

/// Sets what the fake claude writes on its next runs.
fn set_edits(dir: &Path, edits: &[(&str, &str)]) {
    let lines: Vec<String> = edits.iter().map(|(p, c)| format!("{p}|{c}")).collect();
    std::fs::write(dir.join("edits"), lines.join("\n") + "\n").unwrap();
}

/// The folders and argument lists of every fake claude call so far.
fn calls(dir: &Path) -> Vec<(String, Vec<String>)> {
    let log = std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default();
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in log.lines() {
        if let Some(cwd) = line.strip_prefix("cwd=") {
            out.push((cwd.to_string(), Vec::new()));
        } else if let (Some(arg), Some(last)) = (line.strip_prefix("arg="), out.last_mut()) {
            last.1.push(arg.to_string());
        }
    }
    out
}

fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap()
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

impl Daemon {
    async fn start(claude_bin: PathBuf) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        Self::start_in(tmp, home, claude_bin).await
    }

    /// Starts crewd on `home`, inside `tmp`.
    async fn start_in(tmp: TempDir, home: PathBuf, claude_bin: PathBuf) -> Daemon {
        let path = home.clone();
        tokio::spawn(async move {
            let opts = ServeOptions {
                claude_bin: Some(claude_bin),
                crew_mcp_bin: None,
                ..ServeOptions::default()
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

    /// Reads notifications until a run of `thread` reaches `want`.
    async fn until_status_in(&mut self, thread: ThreadId, want: RunStatus) -> Vec<ServerMsg> {
        self.until(|m| {
            matches!(m, ServerMsg::RunStatus { thread: t, status, .. }
                if *t == thread && *status == want)
        })
        .await
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
}

fn agent(name: &str, mode: PermissionMode) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        role: String::new(),
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

/// Forge, Quill and Sleeper may edit; Scout is read-only.
async fn agents(c: &mut Client) {
    for (name, mode) in [
        ("Forge", PermissionMode::EditFiles),
        ("Quill", PermissionMode::EditFiles),
        ("Sleeper", PermissionMode::EditFiles),
        ("Scout", PermissionMode::ReadOnly),
    ] {
        let _: AgentId = c
            .ok(Request::CreateAgent {
                spec: agent(name, mode),
            })
            .await;
    }
}

async fn add_project(
    c: &mut Client,
    root: &Path,
    mode: Option<WorkMode>,
) -> (ProjectId, ChannelId) {
    let project: ProjectId = c
        .ok(Request::CreateProject {
            name: root.file_name().unwrap().to_string_lossy().into_owned(),
            root: root.to_string_lossy().into_owned(),
            main_branch: String::new(),
            work_mode: mode,
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
    (project, channel)
}

async fn new_thread(c: &mut Client, channel: ChannelId, title: &str) -> ThreadId {
    let thread: ThreadId = c
        .ok(Request::CreateThread {
            channel,
            title: title.into(),
        })
        .await;
    c.call(Request::Subscribe { thread }).await.unwrap();
    thread
}

async fn post(c: &mut Client, thread: ThreadId, body: &str) {
    c.call(Request::PostMessage {
        thread,
        body: body.into(),
        skill: None,
        model_override: None,
    })
    .await
    .unwrap();
}

/// Waits for a run of `thread` to reach `want`, and returns it.
async fn until_run(c: &mut Client, thread: ThreadId, want: RunStatus) -> StoredRun {
    let seen = c
        .until(|m| {
            matches!(m, ServerMsg::RunStatus { thread: t, status, .. }
                if *t == thread && *status == want)
        })
        .await;
    match seen.last() {
        Some(ServerMsg::RunStatus { details, .. }) => details.clone(),
        _ => unreachable!(),
    }
}

async fn stored_thread(c: &mut Client, channel: ChannelId, thread: ThreadId) -> StoredThread {
    let threads: Vec<StoredThread> = c.ok(Request::ListThreads { channel }).await;
    threads.into_iter().find(|t| t.id == thread).unwrap()
}

async fn stored_project(c: &mut Client, project: ProjectId) -> StoredProject {
    let projects: Vec<StoredProject> = c.ok(Request::ListProjects).await;
    projects.into_iter().find(|p| p.id == project).unwrap()
}

fn system_notes(seen: &[ServerMsg]) -> Vec<String> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Message { message, .. } if message.author == Author::System => {
                Some(message.body.clone())
            }
            _ => None,
        })
        .collect()
}

/// `platform/` holding the repos `api` and `web`, and a plain README.
fn platform(tmp: &Path) -> PathBuf {
    let root = tmp.join("platform");
    repo(&root.join("api"));
    repo(&root.join("web"));
    std::fs::write(root.join("README.md"), "read me\n").unwrap();
    root
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_folder_says_what_a_folder_holds() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;

    // A repo with ignored setup files.
    let api = d.tmp.path().join("api");
    repo(&api);
    git(
        &api,
        &[
            "remote",
            "add",
            "origin",
            "https://example.com/demo/api.git",
        ],
    );
    std::fs::write(api.join(".gitignore"), ".env*\ncerts/\nnode_modules/\n").unwrap();
    git(&api, &["add", ".gitignore"]);
    git(&api, &["commit", "-q", "-m", "ignore"]);
    std::fs::write(api.join(".env"), "A=1\n").unwrap();
    std::fs::write(api.join(".env.production"), "A=live\n").unwrap();
    std::fs::create_dir_all(api.join("certs")).unwrap();
    std::fs::write(api.join("certs/a.pem"), "x").unwrap();
    std::fs::write(api.join("certs/b.pem"), "y").unwrap();
    std::fs::create_dir_all(api.join("node_modules/pkg")).unwrap();
    std::fs::write(api.join("node_modules/pkg/index.js"), "x").unwrap();

    let info: FolderInfo = c
        .ok(Request::InspectFolder {
            path: api.to_string_lossy().into_owned(),
        })
        .await;
    assert!(info.exists);
    assert_eq!(info.kind, ProjectKind::Repo);
    assert_eq!(info.name_suggestion, "api");
    assert_eq!(info.branch.as_deref(), Some("main"));
    assert_eq!(
        info.remote.as_deref(),
        Some("https://example.com/demo/api.git")
    );
    assert_eq!(info.already_added, None);
    let files: Vec<(&str, bool)> = info
        .setup_files
        .iter()
        .map(|f| (f.path.as_str(), f.looks_secret))
        .collect();
    assert_eq!(
        files,
        [
            (".env", false),
            (".env.production", true),
            ("certs/", false)
        ]
    );
    assert!(info.setup_files.iter().all(|f| f.ignored));
    assert_eq!(info.setup_files[0].size, SetupFileSize::File { bytes: 4 });
    assert_eq!(info.setup_files[2].size, SetupFileSize::Folder { files: 2 });
    assert_eq!(info.root, api.to_string_lossy());

    // A folder inside the repo is the whole repo.
    std::fs::create_dir_all(api.join("src/parse")).unwrap();
    let inside: FolderInfo = c
        .ok(Request::InspectFolder {
            path: api.join("src/parse").to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(inside.kind, ProjectKind::Repo);
    assert_eq!(
        std::path::Path::new(&inside.root).canonicalize().unwrap(),
        api.canonicalize().unwrap()
    );
    assert_eq!(inside.name_suggestion, "api");
    assert_eq!(inside.setup_files, info.setup_files);
    let (api_project, _) = add_project(&mut c, &api, None).await;
    let inside: FolderInfo = c
        .ok(Request::InspectFolder {
            path: api.join("src").to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(inside.already_added, Some(api_project));

    // A folder of repos lists each repo's files under it.
    let root = platform(d.tmp.path());
    std::fs::write(root.join("api/.gitignore"), ".env\n").unwrap();
    std::fs::write(root.join("api/.env"), "A=1\n").unwrap();
    let info: FolderInfo = c
        .ok(Request::InspectFolder {
            path: root.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(
        info.kind,
        ProjectKind::MultiRepo {
            repos: vec!["api".into(), "web".into()]
        }
    );
    assert_eq!(info.branch, None);
    let paths: Vec<&str> = info.setup_files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["api/.env"]);

    // A plain folder, and one already added.
    let notes = d.tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let (project, _) = add_project(&mut c, &notes, None).await;
    let info: FolderInfo = c
        .ok(Request::InspectFolder {
            path: notes.to_string_lossy().into_owned(),
        })
        .await;
    assert_eq!(info.kind, ProjectKind::Plain);
    assert!(info.setup_files.is_empty());
    assert_eq!(info.already_added, Some(project));

    let info: FolderInfo = c
        .ok(Request::InspectFolder {
            path: d.tmp.path().join("missing").to_string_lossy().into_owned(),
        })
        .await;
    assert!(!info.exists);
}

#[tokio::test(flavor = "multi_thread")]
async fn any_folder_can_be_a_project() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;

    let notes = d.tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let refused = c
        .call(Request::CreateProject {
            name: "notes".into(),
            root: notes.to_string_lossy().into_owned(),
            main_branch: String::new(),
            work_mode: Some(WorkMode::OwnCopy),
            copy_files: vec![],
            setup_command: None,
        })
        .await;
    assert_eq!(refused, Err(NEEDS_REPO.to_string()));
    let (plain, _) = add_project(&mut c, &notes, None).await;
    let p = stored_project(&mut c, plain).await;
    assert_eq!(p.kind, ProjectKind::Plain);
    assert_eq!(p.work_mode, WorkMode::InPlace);

    let root = platform(d.tmp.path());
    let multi: ProjectId = c
        .ok(Request::CreateProject {
            name: "platform".into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: String::new(),
            work_mode: None,
            copy_files: vec!["api/.env".into()],
            setup_command: Some("true".into()),
        })
        .await;
    let p = stored_project(&mut c, multi).await;
    assert_eq!(
        p.kind,
        ProjectKind::MultiRepo {
            repos: vec!["api".into(), "web".into()]
        }
    );
    assert_eq!(p.work_mode, WorkMode::OwnCopy);
    assert_eq!(p.copy_files, ["api/.env"]);
    assert_eq!(p.setup_command.as_deref(), Some("true"));

    // A repo on another branch gets that branch as its main one.
    let api = d.tmp.path().join("api");
    repo(&api);
    git(&api, &["checkout", "-q", "-b", "develop"]);
    let (single, _) = add_project(&mut c, &api, None).await;
    let p = stored_project(&mut c, single).await;
    assert_eq!(p.kind, ProjectKind::Repo);
    assert_eq!(p.work_mode, WorkMode::OwnCopy);
    assert_eq!(p.main_branch, "develop");
}

#[tokio::test(flavor = "multi_thread")]
async fn own_copy_of_a_folder_of_repos_works_per_repo() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    let root = platform(d.tmp.path());
    let (_, channel) = add_project(&mut c, &root, None).await;
    let thread = new_thread(&mut c, channel, "Fix login").await;

    set_edits(
        scripts.path(),
        &[("api/a.txt", "api change"), ("web/new.txt", "web file")],
    );
    post(&mut c, thread, "@Forge fix it").await;
    until_run(&mut c, thread, RunStatus::Done).await;

    let folder = real(&d.home).join("worktrees/platform/fix-login");
    let wt = stored_thread(&mut c, channel, thread)
        .await
        .worktree
        .unwrap();
    assert_eq!(wt.path, folder);
    assert_eq!(wt.branch, "crew/fix-login");
    assert!(wt.ready);
    let repos: Vec<&str> = wt.repos.iter().map(|r| r.repo.as_str()).collect();
    assert_eq!(repos, ["api", "web"]);
    for repo in ["api", "web"] {
        assert_eq!(
            git(&folder.join(repo), &["branch", "--show-current"]),
            "crew/fix-login"
        );
    }
    // Plain files at the top are not copied.
    assert!(!folder.join("README.md").exists());
    // The originals are untouched.
    assert_eq!(
        std::fs::read_to_string(root.join("api/a.txt")).unwrap(),
        "one\n"
    );
    assert!(!root.join("web/new.txt").exists());

    // The agent ran in the thread folder and could read the original.
    let (cwd, args) = calls(scripts.path()).pop().unwrap();
    assert_eq!(PathBuf::from(cwd), real(&folder));
    let at = args.iter().position(|a| a == "--add-dir").unwrap();
    assert_eq!(PathBuf::from(&args[at + 1]), root);

    let changes: ThreadChanges = c.ok(Request::ThreadDiff { thread }).await;
    let paths: Vec<&str> = changes.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["api/a.txt", "web/new.txt"]);
    let groups: Vec<(&str, Vec<&str>, Option<&str>)> = changes
        .repos
        .iter()
        .map(|r| {
            (
                r.repo.as_str(),
                r.diff.files.iter().map(|f| f.path.as_str()).collect(),
                r.branch.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            ("api", vec!["a.txt"], Some("crew/fix-login")),
            ("web", vec!["new.txt"], Some("crew/fix-login"))
        ]
    );
    assert!(changes.patch.contains("diff --git a/api/a.txt b/api/a.txt"));
    assert!(changes.outside_git.is_empty());

    // The channel counts both repos.
    let overview: ChannelOverview = c.ok(Request::ChannelOverview { channel }).await;
    assert_eq!(overview.threads[0].files, 2);

    // A commit lands in one repo only.
    let refused = c
        .call(Request::Commit {
            thread,
            repo: None,
            message: "both".into(),
        })
        .await;
    assert!(refused.is_err());
    let sha: String = c
        .ok(Request::Commit {
            thread,
            repo: Some("api".into()),
            message: "fix(api): change a".into(),
        })
        .await;
    assert_eq!(git(&folder.join("api"), &["rev-parse", "HEAD"]), sha);
    assert_eq!(
        git(&folder.join("web"), &["status", "--porcelain"]),
        "?? new.txt"
    );
    let changes: ThreadChanges = c.ok(Request::ThreadDiff { thread }).await;
    let paths: Vec<&str> = changes.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["web/new.txt"]);

    // Discarding web leaves api and its commit.
    c.call(Request::Discard {
        thread,
        repos: Some(vec!["web".into()]),
        delete_branch: false,
    })
    .await
    .unwrap();
    assert!(!folder.join("web").exists());
    assert!(folder.join("api").exists());
    let wt = stored_thread(&mut c, channel, thread)
        .await
        .worktree
        .unwrap();
    let repos: Vec<&str> = wt.repos.iter().map(|r| r.repo.as_str()).collect();
    assert_eq!(repos, ["api"]);
    let web_branches = git(&root.join("web"), &["branch", "--list", "crew/*"]);
    assert_eq!(web_branches, "");

    // Discarding the rest keeps api's branch, which has a commit.
    c.call(Request::Discard {
        thread,
        repos: None,
        delete_branch: false,
    })
    .await
    .unwrap();
    let seen = c
        .until(|m| matches!(m, ServerMsg::Message { message, .. } if message.body.starts_with("Kept branch")))
        .await;
    assert_eq!(
        system_notes(&seen).last().unwrap(),
        "Kept branch crew/fix-login in api because it has commits that aren't merged."
    );
    assert!(!folder.exists());
    assert!(
        stored_thread(&mut c, channel, thread)
            .await
            .worktree
            .is_none()
    );
    assert_eq!(
        git(&root.join("api"), &["branch", "--list", "crew/*"]),
        "crew/fix-login"
    );
    // The originals never changed.
    assert_eq!(git(&root.join("api"), &["status", "--porcelain"]), "");
    assert!(root.join("README.md").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_threads_get_repos_added_since() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    let root = d.tmp.path().join("platform");
    repo(&root.join("api"));
    let (project, channel) = add_project(&mut c, &root, None).await;
    let first = new_thread(&mut c, channel, "First").await;
    set_edits(scripts.path(), &[]);
    post(&mut c, first, "@Forge go").await;
    until_run(&mut c, first, RunStatus::Done).await;

    repo(&root.join("web"));
    let second = new_thread(&mut c, channel, "Second").await;
    post(&mut c, second, "@Forge go").await;
    until_run(&mut c, second, RunStatus::Done).await;

    let repos = |t: StoredThread| -> Vec<String> {
        t.worktree
            .unwrap()
            .repos
            .into_iter()
            .map(|r| r.repo)
            .collect()
    };
    assert_eq!(repos(stored_thread(&mut c, channel, first).await), ["api"]);
    assert_eq!(
        repos(stored_thread(&mut c, channel, second).await),
        ["api", "web"]
    );
    assert_eq!(
        stored_project(&mut c, project).await.kind,
        ProjectKind::MultiRepo {
            repos: vec!["api".into(), "web".into()]
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn in_place_review_commit_and_discard_touch_only_the_runs_files() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    let root = platform(d.tmp.path());
    let (_, channel) = add_project(&mut c, &root, Some(WorkMode::InPlace)).await;
    let thread = new_thread(&mut c, channel, "Tidy up").await;

    set_edits(
        scripts.path(),
        &[
            ("api/a.txt", "agent edit"),
            ("web/b.txt", "agent new file"),
            ("notes.md", "agent notes"),
        ],
    );
    post(&mut c, thread, "@Forge tidy").await;
    until_run(&mut c, thread, RunStatus::Done).await;
    // The agent worked in the folder itself; no worktree was made.
    let (cwd, args) = calls(scripts.path()).pop().unwrap();
    assert_eq!(PathBuf::from(cwd), real(&root));
    assert!(!args.iter().any(|a| a == "--add-dir"));
    assert!(
        stored_thread(&mut c, channel, thread)
            .await
            .worktree
            .is_none()
    );
    assert!(!d.home.join("worktrees/platform").exists());

    // You change files of your own meanwhile.
    std::fs::write(root.join("api/user.txt"), "mine\n").unwrap();
    std::fs::write(root.join("web/a.txt"), "my edit\n").unwrap();

    // Review shows every change, yours marked as outside this thread.
    let changes: ThreadChanges = c.ok(Request::ThreadDiff { thread }).await;
    let paths: Vec<&str> = changes.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        ["api/a.txt", "api/user.txt", "web/a.txt", "web/b.txt"]
    );
    let groups: Vec<(&str, Option<&str>, Vec<&str>)> = changes
        .repos
        .iter()
        .map(|r| {
            (
                r.repo.as_str(),
                r.branch.as_deref(),
                r.outside_thread.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            ("api", Some("main"), vec!["user.txt"]),
            ("web", Some("main"), vec!["a.txt"])
        ]
    );
    // The channel counts only the thread's own changes.
    let overview: ChannelOverview = c.ok(Request::ChannelOverview { channel }).await;
    assert_eq!(overview.threads[0].files, 2);
    let outside: Vec<&str> = changes
        .outside_git
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(outside, ["notes.md"]);

    // A commit takes only the run's file, on the repo's own branch.
    let sha: String = c
        .ok(Request::Commit {
            thread,
            repo: Some("web".into()),
            message: "add b".into(),
        })
        .await;
    let web = root.join("web");
    assert_eq!(git(&web, &["rev-parse", "main"]), sha);
    assert_eq!(
        git(&web, &["show", "--name-only", "--format=", "HEAD"]),
        "b.txt"
    );
    assert_eq!(git(&web, &["status", "--porcelain"]), "M a.txt");

    // A second run edits api/a.txt again and makes a file; you edit too.
    set_edits(
        scripts.path(),
        &[("api/a.txt", "agent again"), ("api/made.txt", "agent made")],
    );
    post(&mut c, thread, "@Forge more").await;
    until_run(&mut c, thread, RunStatus::Done).await;
    std::fs::write(root.join("api/a.txt"), "agent again\n").unwrap();

    c.call(Request::Discard {
        thread,
        repos: None,
        delete_branch: false,
    })
    .await
    .unwrap();
    let seen = c
        .until(
            |m| matches!(m, ServerMsg::Message { message, .. } if message.body.starts_with("Left")),
        )
        .await;
    assert_eq!(
        system_notes(&seen).last().unwrap(),
        "Left api/made.txt as it is: git has no earlier version to put back."
    );
    // The run's tracked file is back as git has it.
    assert_eq!(
        std::fs::read_to_string(root.join("api/a.txt")).unwrap(),
        "one\n"
    );
    // Your files, git's untracked ones and the notes outside git survive.
    assert_eq!(
        std::fs::read_to_string(root.join("api/user.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("web/a.txt")).unwrap(),
        "my edit\n"
    );
    assert!(root.join("api/made.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("notes.md")).unwrap(),
        "agent notes\n"
    );
    assert_eq!(git(&web, &["rev-parse", "main"]), sha);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_folder_works_in_place() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    let notes = d.tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("index.md"), "old\n").unwrap();
    let (_, channel) = add_project(&mut c, &notes, None).await;
    let thread = new_thread(&mut c, channel, "Tidy").await;
    set_edits(scripts.path(), &[("index.md", "new")]);
    post(&mut c, thread, "@Forge tidy").await;
    until_run(&mut c, thread, RunStatus::Done).await;
    assert_eq!(
        std::fs::read_to_string(notes.join("index.md")).unwrap(),
        "new\n"
    );

    let changes: ThreadChanges = c.ok(Request::ThreadDiff { thread }).await;
    assert!(changes.files.is_empty() && changes.repos.is_empty());
    let outside: Vec<&str> = changes
        .outside_git
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(outside, ["index.md"]);
    // Nothing in git, so nothing to commit or put back.
    assert!(
        c.call(Request::Commit {
            thread,
            repo: None,
            message: "m".into()
        })
        .await
        .is_err()
    );
    c.call(Request::Discard {
        thread,
        repos: None,
        delete_branch: false,
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(notes.join("index.md")).unwrap(),
        "new\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn in_place_lets_one_agent_write_at_a_time() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    c.call(Request::SubscribeAll).await.unwrap();
    let notes = d.tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let (_, channel) = add_project(&mut c, &notes, None).await;
    let a = new_thread(&mut c, channel, "One").await;
    let b = new_thread(&mut c, channel, "Two").await;
    let r = new_thread(&mut c, channel, "Three").await;
    set_edits(scripts.path(), &[("b.md", "quill")]);

    post(&mut c, a, "@Sleeper write").await;
    let writer = until_run(&mut c, a, RunStatus::Working).await;
    post(&mut c, b, "@Quill write too").await;
    let seen = c
        .until(|m| {
            matches!(m, ServerMsg::RunStatus { thread, details, .. }
                if *thread == b && details.waiting_for.is_some())
        })
        .await;
    let Some(ServerMsg::RunStatus {
        details, status, ..
    }) = seen.last()
    else {
        unreachable!()
    };
    assert_eq!(*status, RunStatus::Queued);
    assert_eq!(details.waiting_for, Some(writer.id));
    let queued: RunId = details.id;

    // A read-only agent runs alongside.
    post(&mut c, r, "@Scout look").await;
    until_run(&mut c, r, RunStatus::Done).await;
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    let quill = runs.iter().find(|x| x.id == queued).unwrap();
    assert_eq!(quill.status, RunStatus::Queued);

    // Stopping the writer starts the next one.
    c.call(Request::StopRun { run: writer.id }).await.unwrap();
    let started = until_run(&mut c, b, RunStatus::Working).await;
    assert_eq!(started.id, queued);
    assert_eq!(started.waiting_for, None);
    until_run(&mut c, b, RunStatus::Done).await;
    assert_eq!(
        std::fs::read_to_string(notes.join("b.md")).unwrap(),
        "quill\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_writers_mentioned_at_once_take_turns_in_place() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    let notes = d.tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let (_, channel) = add_project(&mut c, &notes, None).await;
    let thread = new_thread(&mut c, channel, "Both").await;
    set_edits(scripts.path(), &[]);
    post(&mut c, thread, "@Sleeper and @Quill, tidy up").await;
    let writer = until_run(&mut c, thread, RunStatus::Working).await;
    let seen = c
        .until(
            |m| matches!(m, ServerMsg::RunStatus { details, .. } if details.waiting_for.is_some()),
        )
        .await;
    let Some(ServerMsg::RunStatus { details, .. }) = seen.last() else {
        unreachable!()
    };
    assert_eq!(details.waiting_for, Some(writer.id));
    c.call(Request::StopRun { run: writer.id }).await.unwrap();
    let next = until_run(&mut c, thread, RunStatus::Done).await;
    assert_eq!(next.id, details.id);
}

/// `api` as a project worked on in place, with Forge and friends.
async fn in_place_api(d: &Daemon, c: &mut Client) -> (PathBuf, ChannelId, ThreadId) {
    agents(c).await;
    let root = d.tmp.path().join("api");
    repo(&root);
    std::fs::write(root.join("b.txt"), "bee\n").unwrap();
    git(&root, &["add", "b.txt"]);
    git(&root, &["commit", "-q", "-m", "b"]);
    let (_, channel) = add_project(c, &root, Some(WorkMode::InPlace)).await;
    let thread = new_thread(c, channel, "Edit").await;
    (root, channel, thread)
}

async fn discard_all(c: &mut Client, thread: ThreadId) {
    c.call(Request::Discard {
        thread,
        repos: None,
        delete_branch: false,
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn in_place_discard_keeps_your_edits_in_files_the_run_touched() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    let (root, _, thread) = in_place_api(&d, &mut c).await;
    // Your work before the run: a staged version and a newer one on disk
    // in a.txt, and a staged edit in b.txt.
    std::fs::write(root.join("a.txt"), "staged\n").unwrap();
    git(&root, &["add", "a.txt"]);
    std::fs::write(root.join("a.txt"), "mine\n").unwrap();
    std::fs::write(root.join("b.txt"), "your b\n").unwrap();
    git(&root, &["add", "b.txt"]);

    set_edits(scripts.path(), &[("a.txt", "agent"), ("b.txt", "agent b")]);
    post(&mut c, thread, "@Forge edit").await;
    until_run(&mut c, thread, RunStatus::Done).await;
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "agent\n"
    );

    // Git's cleanup can't take the copies: they live in the crew home,
    // readable by you only.
    git(&root, &["gc", "-q", "--prune=now"]);
    let snaps = d.home.join("snapshots");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&snaps), 0o700);
    assert_eq!(mode(&snaps.join("runs")), 0o700);
    assert_eq!(mode(&snaps.join("blobs")), 0o700);
    let records = list(&snaps.join("runs"));
    assert_eq!(records.len(), 1);
    assert_eq!(mode(&records[0]), 0o600);
    for copy in list(&snaps.join("blobs")) {
        assert_eq!(mode(&copy), 0o600);
    }
    discard_all(&mut c, thread).await;
    // Nothing of the run is left to undo, so its record goes.
    assert!(list(&snaps.join("runs")).is_empty());
    // Only the run's change is undone; what you had on disk and staged
    // is back as it was.
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(git(&root, &["show", ":a.txt"]), "staged");
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "your b\n"
    );
    assert_eq!(git(&root, &["show", ":b.txt"]), "your b");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_file_too_big_to_copy_is_left_as_is() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    let (root, _, thread) = in_place_api(&d, &mut c).await;
    // Your uncommitted edit, over the copy limit.
    let big = "x".repeat(21 * 1024 * 1024);
    std::fs::write(root.join("a.txt"), &big).unwrap();
    set_edits(scripts.path(), &[("a.txt", "agent")]);
    post(&mut c, thread, "@Forge edit").await;
    until_run(&mut c, thread, RunStatus::Done).await;

    discard_all(&mut c, thread).await;
    let seen = c
        .until(
            |m| matches!(m, ServerMsg::Message { message, .. } if message.body.contains("too big")),
        )
        .await;
    assert_eq!(
        system_notes(&seen).last().unwrap(),
        "a.txt was too big to keep a copy, left as is."
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "agent\n"
    );
}

fn list(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|e| e.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Runs Forge over your own edit in a.txt, then hands `spoil` the copy
/// store and discards. Returns the notes seen.
async fn discard_with_store(spoil: impl FnOnce(&Path)) -> (Vec<String>, PathBuf) {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    let (root, _, thread) = in_place_api(&d, &mut c).await;
    std::fs::write(root.join("a.txt"), "mine\n").unwrap();
    set_edits(scripts.path(), &[("a.txt", "agent")]);
    post(&mut c, thread, "@Forge edit").await;
    until_run(&mut c, thread, RunStatus::Done).await;
    spoil(&d.home.join("snapshots/blobs"));
    discard_all(&mut c, thread).await;
    let seen = c
        .until(|m| matches!(m, ServerMsg::Message { message, .. } if message.body.contains("left as is")))
        .await;
    let text = std::fs::read_to_string(root.join("a.txt")).unwrap();
    assert_eq!(text, "agent\n");
    // Keep the temp folders until the checks are done.
    drop((d, scripts));
    (system_notes(&seen), root)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_or_damaged_copy_is_never_written_back() {
    let (notes, _) = discard_with_store(|store| {
        for copy in list(store) {
            std::fs::remove_file(copy).unwrap();
        }
    })
    .await;
    assert_eq!(
        notes.last().unwrap(),
        "a.txt's copy is missing, left as is."
    );
    let (notes, _) = discard_with_store(|store| {
        for copy in list(store) {
            std::fs::write(copy, "something else\n").unwrap();
        }
    })
    .await;
    assert_eq!(
        notes.last().unwrap(),
        "a.txt's copy is damaged, left as is."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn files_left_out_of_a_snapshot_are_named_in_the_thread() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    let (root, channel, thread) = in_place_api(&d, &mut c).await;
    // An untracked file over 1 MB is not copied.
    std::fs::write(root.join("dump.bin"), "z".repeat(2 * 1024 * 1024)).unwrap();
    set_edits(scripts.path(), &[("a.txt", "agent")]);
    post(&mut c, thread, "@Forge edit").await;
    let seen = c.until_status_in(thread, RunStatus::Done).await;
    assert!(
        system_notes(&seen)
            .contains(&"Crew couldn't keep a copy of 1 file, so it won't undo it.".to_string()),
        "{:?}",
        system_notes(&seen)
    );
    let blobs = list(&d.home.join("snapshots/blobs"));
    assert!(blobs.is_empty(), "{blobs:?}");
    // Archiving the thread drops its record.
    assert_eq!(list(&d.home.join("snapshots/runs")).len(), 1);
    c.call(Request::ArchiveThread {
        thread,
        archived: true,
    })
    .await
    .unwrap();
    assert!(list(&d.home.join("snapshots/runs")).is_empty());
    let _ = channel;
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_through_a_linked_repos_real_path_are_flagged() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    let root = d.tmp.path().join("platform");
    repo(&root.join("deep/a/b/web"));
    std::os::unix::fs::symlink(root.join("deep/a/b/web"), root.join("web")).unwrap();
    let (project, channel) = add_project(&mut c, &root, Some(WorkMode::InPlace)).await;
    assert_eq!(
        stored_project(&mut c, project).await.kind,
        ProjectKind::MultiRepo {
            repos: vec!["web".into()]
        }
    );
    let thread = new_thread(&mut c, channel, "Through").await;
    set_edits(scripts.path(), &[("deep/a/b/web/x.txt", "agent")]);
    post(&mut c, thread, "@Forge edit").await;
    let seen = c.until_status_in(thread, RunStatus::Done).await;
    assert!(
        system_notes(&seen)
            .contains(&"Edits through deep/a/b/web/x.txt aren't tracked by Crew.".to_string()),
        "{:?}",
        system_notes(&seen)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn in_place_discard_skips_files_changed_after_the_run() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    let (root, _, thread) = in_place_api(&d, &mut c).await;
    set_edits(scripts.path(), &[("a.txt", "agent"), ("b.txt", "agent b")]);
    post(&mut c, thread, "@Forge edit").await;
    until_run(&mut c, thread, RunStatus::Done).await;
    // You change a.txt after the run.
    std::fs::write(root.join("a.txt"), "agent, then you\n").unwrap();

    discard_all(&mut c, thread).await;
    let seen = c
        .until(|m| matches!(m, ServerMsg::Message { message, .. } if message.body.contains("changed after the run")))
        .await;
    assert_eq!(
        system_notes(&seen).last().unwrap(),
        "a.txt changed after the run, left as is."
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "agent, then you\n"
    );
    // b.txt was clean before the run, so it is back as git has it.
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "bee\n"
    );
    assert_eq!(git(&root, &["status", "--porcelain"]), "M a.txt");
}

#[tokio::test(flavor = "multi_thread")]
async fn in_place_commit_and_discard_wait_for_a_writer() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    let (_, channel, thread) = in_place_api(&d, &mut c).await;
    set_edits(scripts.path(), &[("a.txt", "agent")]);
    post(&mut c, thread, "@Forge edit").await;
    until_run(&mut c, thread, RunStatus::Done).await;

    let other = new_thread(&mut c, channel, "Other").await;
    post(&mut c, other, "@Sleeper write").await;
    let writer = until_run(&mut c, other, RunStatus::Working).await;
    let wait = Err("Wait for Sleeper to finish writing, or stop it.".to_string());
    let commit = c
        .call(Request::Commit {
            thread,
            repo: None,
            message: "m".into(),
        })
        .await;
    assert_eq!(commit.map(|_| ()), wait);
    let discard = c
        .call(Request::Discard {
            thread,
            repos: None,
            delete_branch: false,
        })
        .await;
    assert_eq!(discard.map(|_| ()), wait);

    c.call(Request::StopRun { run: writer.id }).await.unwrap();
    until_run(&mut c, other, RunStatus::Stopped).await;
    let _: String = c
        .ok(Request::Commit {
            thread,
            repo: None,
            message: "m".into(),
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_gone_from_the_folder_still_lets_a_thread_be_discarded() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    let root = platform(d.tmp.path());
    let (project, channel) = add_project(&mut c, &root, None).await;
    let thread = new_thread(&mut c, channel, "Old").await;
    set_edits(scripts.path(), &[("web/a.txt", "web change")]);
    post(&mut c, thread, "@Forge go").await;
    until_run(&mut c, thread, RunStatus::Done).await;
    let folder = real(&d.home).join("worktrees/platform/old");
    assert!(folder.join("web").exists());

    std::fs::remove_dir_all(root.join("web")).unwrap();
    // Opening the project sees the folder as it is now.
    let _: serde_json::Value = c
        .ok(Request::ProjectOverview {
            project,
            light: false,
        })
        .await;
    assert_eq!(
        stored_project(&mut c, project).await.kind,
        ProjectKind::MultiRepo {
            repos: vec!["api".into()]
        }
    );
    // The old thread keeps its worktrees, and review skips the lost repo.
    let wt = stored_thread(&mut c, channel, thread)
        .await
        .worktree
        .unwrap();
    assert_eq!(wt.repos.len(), 2);
    let changes: ThreadChanges = c.ok(Request::ThreadDiff { thread }).await;
    assert!(changes.repos.iter().all(|r| r.repo == "api"));

    discard_all(&mut c, thread).await;
    assert!(!folder.exists());
    assert!(
        stored_thread(&mut c, channel, thread)
            .await
            .worktree
            .is_none()
    );
    assert_eq!(git(&root.join("api"), &["branch", "--list", "crew/*"]), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_replaced_by_another_clone_still_lets_a_thread_be_discarded() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    agents(&mut c).await;
    let root = platform(d.tmp.path());
    let (_, channel) = add_project(&mut c, &root, None).await;
    let thread = new_thread(&mut c, channel, "Old").await;
    set_edits(scripts.path(), &[("web/a.txt", "web change")]);
    post(&mut c, thread, "@Forge go").await;
    until_run(&mut c, thread, RunStatus::Done).await;
    let folder = real(&d.home).join("worktrees/platform/old");
    assert!(folder.join("web").exists());

    // `web` is now a different clone at the same path.
    std::fs::remove_dir_all(root.join("web")).unwrap();
    repo(&root.join("web"));
    std::fs::write(root.join("web/mine.txt"), "mine\n").unwrap();

    discard_all(&mut c, thread).await;
    assert!(!folder.exists(), "Crew's own copy goes");
    assert!(
        stored_thread(&mut c, channel, thread)
            .await
            .worktree
            .is_none()
    );
    // The new clone is untouched and knows no stale worktree.
    assert_eq!(
        std::fs::read_to_string(root.join("web/mine.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        git(&root.join("web"), &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_folder_is_refused() {
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start(fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    let got = c
        .call(Request::CreateProject {
            name: "gone".into(),
            root: d.tmp.path().join("gone").to_string_lossy().into_owned(),
            main_branch: String::new(),
            work_mode: None,
            copy_files: vec![],
            setup_command: None,
        })
        .await;
    assert_eq!(got, Err("Folder not found.".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn existing_projects_learn_their_kind_on_start() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir_all(&home).unwrap();
    let root = platform(tmp.path());
    {
        let store = crew_core::store::Store::open(&home.join("crew.db")).unwrap();
        store.create_project("platform", &root, "main").unwrap();
    }
    let scripts = tempfile::tempdir().unwrap();
    let d = Daemon::start_in(tmp, home, fake_claude(scripts.path())).await;
    let mut c = d.client().await;
    // crewd answers while it looks, and learns the kind soon after.
    let want = ProjectKind::MultiRepo {
        repos: vec!["api".into(), "web".into()],
    };
    let started = std::time::Instant::now();
    let project = loop {
        let projects: Vec<StoredProject> = c.ok(Request::ListProjects).await;
        if projects[0].kind == want {
            break projects[0].clone();
        }
        assert!(started.elapsed() < WAIT, "the kind wasn't learnt");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(project.work_mode, WorkMode::OwnCopy);
}
