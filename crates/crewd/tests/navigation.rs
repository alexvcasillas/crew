//! Search, the project overview, the agent profile and project rename
//! and removal, through a real crewd and fake CLIs.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::store::{ApprovalStatus, Author, Limits, StoredRun};
use crew_proto::{
    AgentProfile, ClientMsg, FileHit, PROTO_VERSION, ProjectOverview, Request, RunPlace,
    SearchResults, ServerMsg, SetupFileSize, ThreadState, ThreadSummary, read_frame, write_frame,
};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(15);

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

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

/// A fake claude that writes `file.txt` where it runs, then replays a
/// run with one edit that ends `Done`.
fn writing_claude(dir: &Path) -> PathBuf {
    script(
        &dir.join("claude-writes.sh"),
        &format!(
            "exec '{}' '{}'\n",
            fake("claude_writes.sh").display(),
            fixture("edit_and_test.jsonl").display()
        ),
    )
}

/// A fake claude that starts a session, then waits until it is stopped.
fn sleeping_claude(dir: &Path) -> PathBuf {
    script(
        &dir.join("claude-sleeps.sh"),
        &format!(
            "head -n 1 '{}'\nsleep 60\n",
            fixture("read_file.jsonl").display()
        ),
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

/// A repo on `main` holding `files`, with `node_modules/` and `target/`
/// ignored and full of files.
fn repo(dir: &Path, files: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join(".gitignore"), "node_modules/\ntarget/\n").unwrap();
    for f in files {
        let path = dir.join(f);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{f}\n")).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
    for ignored in ["node_modules/parser", "target/debug/parser"] {
        std::fs::create_dir_all(dir.join(ignored)).unwrap();
        std::fs::write(dir.join(ignored).join("parser.js"), "x").unwrap();
    }
}

struct Daemon {
    tmp: TempDir,
    home: PathBuf,
}

impl Daemon {
    async fn start(claude_bin: PathBuf) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("crew");
        let path = home.clone();
        tokio::spawn(async move {
            let opts = ServeOptions {
                claude_bin: Some(claude_bin),
                codex_bin: Some(PathBuf::from("/nonexistent/codex")),
                crew_mcp_bin: None,
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

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
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
    async fn call(&mut self, req: Request) -> Result<serde_json::Value, String> {
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

    async fn project(&mut self, name: &str, root: &Path) -> ProjectId {
        self.ok(Request::CreateProject {
            name: name.into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
            work_mode: None,
            copy_files: vec![],
            setup_command: None,
        })
        .await
    }

    async fn channel(&mut self, project: ProjectId, name: &str) -> ChannelId {
        self.ok(Request::CreateChannel {
            project,
            name: name.into(),
        })
        .await
    }

    async fn thread(&mut self, channel: ChannelId, title: &str) -> ThreadId {
        self.ok(Request::CreateThread {
            channel,
            title: title.into(),
        })
        .await
    }

    async fn agent(&mut self, name: &str, mode: PermissionMode) -> AgentId {
        self.ok(Request::CreateAgent {
            spec: AgentSpec {
                name: name.into(),
                role: "Builds things".into(),
                runtime: Runtime::ClaudeCode,
                model: "claude-haiku-4-5".into(),
                instructions: String::new(),
                permission_mode: mode,
                allowed_tools: vec![],
                handoff_policy: None,
                handoff_budget: None,
                max_run_minutes: 20,
            },
        })
        .await
    }

    async fn post(&mut self, thread: ThreadId, body: &str) {
        self.call(Request::PostMessage {
            thread,
            body: body.into(),
        })
        .await
        .unwrap();
    }

    async fn search(&mut self, query: &str, project: Option<ProjectId>) -> SearchResults {
        self.ok(Request::Search {
            query: query.into(),
            project,
            limit: 20,
        })
        .await
    }

    /// Waits for a run of `thread` to reach `status`.
    async fn run_reaches(&mut self, thread: ThreadId, status: RunStatus) -> RunId {
        let mut run = None;
        self.until(|m| match m {
            ServerMsg::RunStatus {
                thread: t,
                run: r,
                status: s,
                ..
            } if *t == thread && *s == status => {
                run = Some(*r);
                true
            }
            _ => false,
        })
        .await;
        run.unwrap()
    }
}

fn titles(r: &SearchResults) -> Vec<&str> {
    r.threads.iter().map(|t| t.title.as_str()).collect()
}

fn file<'a>(r: &'a SearchResults, path: &str) -> Vec<&'a FileHit> {
    r.files.iter().filter(|f| f.path == path).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn search_ranks_threads_channels_agents_and_files() {
    let bins = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_claude(bins.path())).await;
    let root = d.path("demo");
    repo(&root, &["src/parser.rs", "src/lexer.rs", "docs/reparse.md"]);
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    let general = c.channel(p, "general").await;
    let parsers = c.channel(p, "parsers").await;
    let _ = c.channel(p, "docs").await;
    let speed = c.thread(general, "Speed up the reparse").await;
    let fix = c.thread(general, "Fix the parser").await;
    let crash = c.thread(parsers, "Parser crash on empty input").await;
    let lexer = c.thread(general, "Lexer cleanup").await;
    let old = c.thread(general, "Parser of old").await;
    c.call(Request::ArchiveThread {
        thread: old,
        archived: true,
    })
    .await
    .unwrap();
    let parsey = c.agent("Parsey", PermissionMode::ReadOnly).await;
    let gone = c.agent("Parsed", PermissionMode::ReadOnly).await;
    c.call(Request::DeleteAgent { agent: gone }).await.unwrap();

    // Another project's thread only shows when searching everywhere.
    let other_root = d.path("other");
    repo(&other_root, &["parse.c"]);
    let q = c.project("other", &other_root).await;
    let other_general = c.channel(q, "general").await;
    let elsewhere = c.thread(other_general, "parse it elsewhere").await;

    let r = c.search("PARSE", Some(p)).await;
    // Prefix, then word start, then anywhere. Archived threads stay out.
    assert_eq!(
        titles(&r),
        [
            "Parser crash on empty input",
            "Fix the parser",
            "Speed up the reparse"
        ]
    );
    assert_eq!(r.threads[0].thread, crash);
    let r_threads = (r.threads[1].thread, r.threads[2].thread);
    assert_eq!(r.threads[0].channel, parsers);
    assert_eq!(r.threads[0].state, ThreadState::Idle);
    assert_eq!(r.channels, [parsers]);
    assert_eq!(r.agents, [parsey], "deleted agents stay out");
    let paths: Vec<&str> = r.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["src/parser.rs", "docs/reparse.md"]);
    assert!(r.files.iter().all(|f| f.thread.is_none() && !f.changed));
    assert!(r.files.iter().all(|f| f.project == p));
    assert_eq!(r.totals.threads, 3);
    assert_eq!(r.totals.files, 2);

    let all = c.search("parse", None).await;
    assert!(all.threads.iter().any(|t| t.thread == elsewhere));
    assert!(
        all.files
            .iter()
            .any(|f| f.project == q && f.path == "parse.c")
    );
    let limited: SearchResults = c
        .ok(Request::Search {
            query: "parse".into(),
            project: None,
            limit: 1,
        })
        .await;
    assert_eq!(limited.threads.len(), 1);
    assert_eq!(limited.totals.threads, 4);

    // Message text finds a thread too, after the title matches.
    c.post(lexer, "the tokenizer breaks on tabs").await;
    let r = c.search("tokenizer", Some(p)).await;
    assert_eq!(titles(&r), ["Lexer cleanup"]);
    assert!(c.search("", Some(p)).await.threads.is_empty());

    // A run writes a file in its worktree: once it ends, the file is
    // found there as changed.
    assert_eq!(
        [r_threads.0, r_threads.1],
        [fix, speed],
        "word start before anywhere"
    );
    c.agent("Forge", PermissionMode::Auto).await;
    c.call(Request::SubscribeAll).await.unwrap();
    c.post(crash, "@Forge go").await;
    c.run_reaches(crash, RunStatus::Done).await;
    let r = c.search("file.txt", Some(p)).await;
    let hits = file(&r, "file.txt");
    assert_eq!(hits.len(), 1, "{r:?}");
    assert_eq!(hits[0].thread, Some(crash));
    assert!(hits[0].changed);
    // The worktree's unchanged files don't repeat the root's.
    assert_eq!(
        file(&c.search("lexer.rs", Some(p)).await, "src/lexer.rs").len(),
        1
    );

    // Committed, it is no longer changed.
    c.call(Request::Commit {
        thread: crash,
        repo: None,
        message: "Add file".into(),
    })
    .await
    .unwrap();
    let r = c.search("file.txt", Some(p)).await;
    let hits = file(&r, "file.txt");
    assert_eq!(hits.len(), 1);
    assert!(!hits[0].changed);
    assert_eq!(r.threads.len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_hits_say_who_works_and_who_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Daemon::start(sleeping_claude(tmp.path())).await;
    let root = d.path("demo");
    repo(&root, &["a.txt"]);
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    let general = c.channel(p, "general").await;
    let t = c.thread(general, "Long job").await;
    let forge = c.agent("Forge", PermissionMode::ReadOnly).await;
    c.call(Request::SubscribeAll).await.unwrap();
    c.post(t, "@Forge go").await;
    c.run_reaches(t, RunStatus::Working).await;
    let r = c.search("long", None).await;
    assert_eq!(r.threads[0].state, ThreadState::Working { agent: forge });
    c.call(Request::StopThread { thread: t }).await.unwrap();
    c.run_reaches(t, RunStatus::Stopped).await;
    let r = c.search("long", None).await;
    assert_eq!(r.threads[0].state, ThreadState::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn project_overview_sums_up_the_project() {
    let bins = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_claude(bins.path())).await;
    let root = d.path("demo");
    repo(&root, &["src/lib.rs"]);
    std::fs::write(root.join(".env"), "KEY=1\n").unwrap();
    std::fs::create_dir_all(root.join("certs")).unwrap();
    std::fs::write(root.join("certs/a.pem"), "a").unwrap();
    std::fs::write(root.join("certs/b.pem"), "b").unwrap();
    git(
        &root,
        &["remote", "add", "origin", "https://example.com/demo.git"],
    );
    std::fs::write(root.join("src/lib.rs"), "edited\n").unwrap();

    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    c.call(Request::UpdateProject {
        project: p,
        setup_command: None,
        copy_files: vec![".env".into(), "certs".into(), "missing.txt".into()],
    })
    .await
    .unwrap();
    let general = c.channel(p, "general").await;
    let bugs = c.channel(p, "bugs").await;
    let first = c.thread(general, "First").await;
    let second = c.thread(bugs, "Second").await;
    let _idle = c.thread(bugs, "Idle").await;
    let forge = c.agent("Forge", PermissionMode::Auto).await;
    let scout = c.agent("Scout", PermissionMode::ReadOnly).await;
    c.call(Request::SubscribeAll).await.unwrap();
    c.post(first, "@Forge go").await;
    c.run_reaches(first, RunStatus::Done).await;
    c.post(second, "@Forge go").await;
    c.run_reaches(second, RunStatus::Done).await;
    c.post(second, "@Scout look").await;
    c.run_reaches(second, RunStatus::Done).await;
    // A deleted agent still shows as used here.
    c.call(Request::DeleteAgent { agent: scout }).await.unwrap();

    let o: ProjectOverview = c.ok(Request::ProjectOverview { project: p }).await;
    assert!(o.root.exists);
    assert_eq!(o.root.branch.as_deref(), Some("main"));
    // src/lib.rs, plus .env and the two files in certs, which git
    // doesn't ignore here.
    assert!(o.root.is_repo);
    assert_eq!(o.root.uncommitted, 4);
    assert_eq!(
        o.root.remote_url.as_deref(),
        Some("https://example.com/demo.git")
    );
    assert_eq!(o.worktrees, 2);
    let channels: Vec<ChannelId> = o.channels.iter().map(|c| c.channel).collect();
    assert_eq!(channels, [general, bugs]);
    let bug_stats = &o.channels[1].stats;
    assert_eq!(bug_stats.threads, 2);
    assert_eq!(bug_stats.runs_week, 2);
    assert_eq!(bug_stats.files_week, 1, "file.txt in the worktree");
    let recent: Vec<ThreadId> = o.recent_threads.iter().map(|t| t.thread).collect();
    assert_eq!(recent[..2], [second, first]);
    assert_eq!(o.recent_threads.len(), 3);
    assert_eq!(o.recent_threads[0].agents, [forge, scout]);
    assert_eq!(o.recent_threads[0].state, ThreadState::Done);
    assert_eq!(o.recent_threads[2].state, ThreadState::Idle);
    let used: Vec<(AgentId, u32)> = o.agents.iter().map(|a| (a.agent, a.runs_week)).collect();
    assert_eq!(used, [(forge, 2), (scout, 1)]);
    let setup: Vec<(&str, SetupFileSize)> = o
        .setup_files
        .iter()
        .map(|f| (f.path.as_str(), f.size))
        .collect();
    assert_eq!(
        setup,
        [
            (".env", SetupFileSize::File { bytes: 6 }),
            ("certs", SetupFileSize::Folder { files: 2 }),
            ("missing.txt", SetupFileSize::Missing),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_whose_folder_is_gone_has_an_overview_and_can_be_removed() {
    let d = Daemon::start(PathBuf::from("/nonexistent/claude")).await;
    let root = d.path("demo");
    repo(&root, &["a.txt"]);
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    let general = c.channel(p, "general").await;
    c.thread(general, "Something").await;
    std::fs::remove_dir_all(&root).unwrap();

    let o: ProjectOverview = c.ok(Request::ProjectOverview { project: p }).await;
    assert!(!o.root.exists);
    assert_eq!(o.root.branch, None);
    assert_eq!(o.channels.len(), 1);
    assert_eq!(o.channels[0].stats.threads, 1);
    assert!(c.search("a.txt", Some(p)).await.files.is_empty());

    c.call(Request::RemoveProject { project: p }).await.unwrap();
    let projects: Vec<serde_json::Value> = c.ok(Request::ListProjects).await;
    assert!(projects.is_empty());
    assert!(
        c.call(Request::ProjectOverview { project: p })
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_profile_counts_runs_edits_and_threads() {
    let bins = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_claude(bins.path())).await;
    let root = d.path("demo");
    repo(&root, &["a.txt"]);
    let other_root = d.path("other");
    repo(&other_root, &["b.txt"]);
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    let q = c.project("other", &other_root).await;
    let general = c.channel(p, "general").await;
    let elsewhere = c.channel(q, "general").await;
    let first = c.thread(general, "First").await;
    let second = c.thread(elsewhere, "Second").await;
    let archived = c.thread(general, "Archived").await;
    let forge = c.agent("Forge", PermissionMode::Auto).await;
    c.call(Request::SubscribeAll).await.unwrap();
    for t in [first, second, archived] {
        c.post(t, "@Forge go").await;
        c.run_reaches(t, RunStatus::Done).await;
    }
    c.call(Request::ArchiveThread {
        thread: archived,
        archived: true,
    })
    .await
    .unwrap();

    let profile: AgentProfile = c.ok(Request::AgentProfile { agent: forge }).await;
    let s = profile.stats;
    assert_eq!((s.runs_week, s.runs_last_week), (3, 0));
    assert_eq!((s.finished_week, s.clean_week), (3, 3));
    assert!(s.avg_run_ms >= 0);
    // Each run's one edit: a line in src/lib.rs.
    assert_eq!((s.files_week, s.added_week, s.removed_week), (3, 3, 3));
    assert_eq!(profile.projects, 2);
    let runs: Vec<ThreadId> = profile.recent_runs.iter().map(|r| r.run.thread).collect();
    assert_eq!(runs, [archived, second, first]);
    let row = &profile.recent_runs[0];
    assert_eq!((row.files, row.added, row.removed), (1, 1, 1));
    assert_eq!(row.run.status, RunStatus::Done);
    assert!(!row.waiting);
    assert_eq!(profile.threads, [second, first]);
    assert!(
        profile
            .recent_runs
            .iter()
            .all(|r| r.removed_project.is_none())
    );

    // A run in a removed project says where it ran.
    c.call(Request::RemoveProject { project: q }).await.unwrap();
    let profile: AgentProfile = c.ok(Request::AgentProfile { agent: forge }).await;
    let places: Vec<_> = profile
        .recent_runs
        .iter()
        .map(|r| r.removed_project.clone())
        .collect();
    assert_eq!(
        places,
        [
            None,
            Some(RunPlace {
                project: "other".into(),
                channel: "general".into(),
                thread_title: "Second".into(),
            }),
            None,
        ]
    );
    assert_eq!(profile.projects, 1);

    // Once deleted, it still has a profile, history and all.
    c.call(Request::DeleteAgent { agent: forge }).await.unwrap();
    let after: AgentProfile = c.ok(Request::AgentProfile { agent: forge }).await;
    assert_eq!(after.recent_runs.len(), 3);
    assert!(
        c.call(Request::AgentProfile {
            agent: AgentId::new()
        })
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn renaming_a_project_is_broadcast() {
    let d = Daemon::start(PathBuf::from("/nonexistent/claude")).await;
    let root = d.path("demo");
    std::fs::create_dir_all(&root).unwrap();
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    c.call(Request::SubscribeAll).await.unwrap();
    c.call(Request::RenameProject {
        project: p,
        name: "  Demo app ".into(),
    })
    .await
    .unwrap();
    c.until(|m| {
        matches!(m, ServerMsg::ProjectChanged { project } if project.id == p && project.name == "Demo app")
    })
    .await;
    let err = c
        .call(Request::RenameProject {
            project: p,
            name: " ".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err, "Give the project a name.");
}

#[tokio::test(flavor = "multi_thread")]
async fn projects_may_not_share_a_folder() {
    let d = Daemon::start(PathBuf::from("/nonexistent/claude")).await;
    let root = d.path("demo");
    std::fs::create_dir_all(&root).unwrap();
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    let again = c
        .call(Request::CreateProject {
            name: "again".into(),
            root: format!("{}/", root.display()),
            main_branch: "main".into(),
            work_mode: None,
            copy_files: vec![],
            setup_command: None,
        })
        .await
        .unwrap_err();
    assert_eq!(again, "demo already uses this folder.");
    // Once removed, adding the folder brings the same project back.
    c.call(Request::RemoveProject { project: p }).await.unwrap();
    assert_eq!(c.project("again", &root).await, p);
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_project_drops_its_queue_and_keeps_its_files() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Daemon::start(sleeping_claude(tmp.path())).await;
    let root = d.path("demo");
    repo(&root, &["a.txt"]);
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    let general = c.channel(p, "general").await;
    let busy = c.thread(general, "Busy").await;
    let waiting = c.thread(general, "Waiting").await;
    c.agent("Forge", PermissionMode::Auto).await;
    let keep_root = d.path("keep");
    repo(&keep_root, &["k.txt"]);
    let keep = c.project("keep", &keep_root).await;
    c.call(Request::SubscribeAll).await.unwrap();

    // A live run: refused.
    c.post(busy, "@Forge go").await;
    let mut worktree = None;
    c.until(|m| match m {
        ServerMsg::ThreadChanged { thread } if thread.id == busy => {
            worktree = thread.worktree.clone().filter(|w| w.ready);
            worktree.is_some()
        }
        _ => false,
    })
    .await;
    let worktree = worktree.unwrap();
    let err = c
        .call(Request::RemoveProject { project: p })
        .await
        .unwrap_err();
    assert_eq!(err, "Stop the agents in demo first.");
    c.call(Request::StopThread { thread: busy }).await.unwrap();
    c.run_reaches(busy, RunStatus::Stopped).await;

    // A queued run: dropped with a note.
    let limits = Limits {
        paused: true,
        ..Limits::default()
    };
    c.call(Request::SetLimits { limits }).await.unwrap();
    c.post(waiting, "@Forge later").await;
    c.run_reaches(waiting, RunStatus::Queued).await;
    c.call(Request::RemoveProject { project: p }).await.unwrap();
    let seen = c
        .until(|m| matches!(m, ServerMsg::ProjectRemoved { project } if *project == p))
        .await;
    assert!(seen.iter().any(|m| matches!(
        m,
        ServerMsg::RunStatus { thread, status: RunStatus::Stopped, .. } if *thread == waiting
    )));
    assert!(seen.iter().any(|m| matches!(
        m,
        ServerMsg::Message { thread, message } if *thread == waiting
            && message.author == Author::System
            && message.body == "Stopped before starting."
    )));

    // Gone from every view, still on disk.
    let ServerMsg::Snapshot {
        projects,
        channels,
        threads,
        runs,
        ..
    } = ({
        let mut fresh = d.client().await;
        fresh.call(Request::SubscribeAll).await.unwrap();
        fresh.recv().await
    })
    else {
        panic!("no snapshot");
    };
    assert_eq!(
        projects.iter().map(|p| p.id).collect::<Vec<_>>(),
        [keep],
        "only the other project"
    );
    assert!(channels.iter().all(|ch| ch.project == keep));
    assert!(threads.is_empty());
    assert!(runs.is_empty());
    let history: Vec<StoredRun> = c
        .ok(Request::RunHistory {
            before: None,
            limit: 50,
        })
        .await;
    // The stopped run stays in the history; the queued one never started.
    assert_eq!(
        history.iter().map(|r| r.thread).collect::<Vec<_>>(),
        [busy],
        "runs stay for history"
    );
    assert!(c.search("waiting", None).await.threads.is_empty());
    assert!(root.join("a.txt").exists());
    assert!(worktree.path.is_dir(), "the worktree stays");
    assert!(
        git(&root, &["branch", "--list", &worktree.branch]).contains(&worktree.branch),
        "the branch stays"
    );
    assert!(c.call(Request::RemoveProject { project: p }).await.is_err());

    // Nothing is written into it any more, by this client or another.
    let refused = "That project was removed.".to_string();
    for req in [
        Request::PostMessage {
            thread: busy,
            body: "@Forge again".into(),
        },
        Request::CreateThread {
            channel: general,
            title: "New".into(),
        },
        Request::CreateChannel {
            project: p,
            name: "more".into(),
        },
        Request::RenameThread {
            thread: busy,
            title: "Renamed".into(),
        },
        Request::Commit {
            thread: busy,
            repo: None,
            message: "x".into(),
        },
        Request::RemoveProject { project: p },
    ] {
        assert_eq!(c.call(req).await, Err(refused.clone()));
    }
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert!(
        runs.iter().all(|r| r.status != RunStatus::Queued),
        "no run was queued"
    );
}

/// A fake claude that edits like `writing_claude` and replies asking
/// Scout to check its work, which asks you first.
fn mentioning_claude(dir: &Path) -> PathBuf {
    let fixture_text = std::fs::read_to_string(fixture("edit_and_test.jsonl"))
        .unwrap()
        .replace("\"Fixed.\"", "\"Fixed. @Scout please check it.\"");
    let path = dir.join("mentions.jsonl");
    std::fs::write(&path, fixture_text).unwrap();
    script(
        &dir.join("claude-mentions.sh"),
        &format!(
            "exec '{}' '{}'\n",
            fake("claude_ok.sh").display(),
            path.display()
        ),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_project_closes_its_asks_and_adding_it_again_brings_it_back() {
    let bins = tempfile::tempdir().unwrap();
    let d = Daemon::start(mentioning_claude(bins.path())).await;
    let root = d.path("demo");
    repo(&root, &["a.txt"]);
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    let general = c.channel(p, "general").await;
    let t = c.thread(general, "Fix it").await;
    c.agent("Forge", PermissionMode::Auto).await;
    c.agent("Scout", PermissionMode::ReadOnly).await;
    c.call(Request::SubscribeAll).await.unwrap();
    c.call(Request::Subscribe { thread: t }).await.unwrap();
    c.post(t, "@Forge go").await;
    c.run_reaches(t, RunStatus::Done).await;
    c.until(|m| matches!(m, ServerMsg::Approval { approval, .. } if approval.status == ApprovalStatus::Pending))
        .await;

    c.call(Request::RemoveProject { project: p }).await.unwrap();
    let seen = c
        .until(|m| matches!(m, ServerMsg::ProjectRemoved { project } if *project == p))
        .await;
    assert!(seen.iter().any(|m| matches!(
        m,
        ServerMsg::Approval { approval, .. } if approval.status == ApprovalStatus::Cancelled
    )));
    assert!(seen.iter().any(|m| matches!(
        m,
        ServerMsg::Message { message, .. }
            if message.body == "Closed because the project was removed."
    )));
    let pending: Vec<serde_json::Value> = c.ok(Request::ListApprovals { thread: None }).await;
    assert!(pending.is_empty(), "nothing waits for you any more");
    assert!(
        c.call(Request::ProjectOverview { project: p })
            .await
            .is_err()
    );

    // Adding the folder again brings the project back as it was, its
    // recent runs included.
    assert_eq!(c.project("demo again", &root).await, p);
    c.until(|m| matches!(m, ServerMsg::RunStatus { thread, .. } if *thread == t))
        .await;
    let o: ProjectOverview = c.ok(Request::ProjectOverview { project: p }).await;
    assert_eq!(o.channels[0].channel, general);
    assert_eq!(o.recent_threads[0].thread, t);
    let r = c.search("fix it", Some(p)).await;
    assert_eq!(r.threads[0].thread, t);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_adding_one_folder_make_one_project() {
    let d = Daemon::start(PathBuf::from("/nonexistent/claude")).await;
    let root = d.path("demo");
    std::fs::create_dir_all(&root).unwrap();
    let mut clients = Vec::new();
    for _ in 0..4 {
        clients.push(d.client().await);
    }
    let adds = clients.into_iter().map(|mut c| {
        let root = root.clone();
        tokio::spawn(async move {
            c.call(Request::CreateProject {
                name: "demo".into(),
                root: root.to_string_lossy().into_owned(),
                main_branch: "main".into(),
                work_mode: None,
                copy_files: vec![],
                setup_command: None,
            })
            .await
        })
    });
    let mut made = 0;
    for add in adds.collect::<Vec<_>>() {
        if add.await.unwrap().is_ok() {
            made += 1;
        }
    }
    assert_eq!(made, 1);
    let mut c = d.client().await;
    let projects: Vec<serde_json::Value> = c.ok(Request::ListProjects).await;
    assert_eq!(projects.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn pinning_a_project_is_broadcast_and_kept() {
    let d = Daemon::start(PathBuf::from("/nonexistent/claude")).await;
    let root = d.path("demo");
    std::fs::create_dir_all(&root).unwrap();
    let mut c = d.client().await;
    let p = c.project("demo", &root).await;
    c.call(Request::SubscribeAll).await.unwrap();
    c.call(Request::PinProject {
        project: p,
        pinned: true,
    })
    .await
    .unwrap();
    c.until(|m| {
        matches!(m, ServerMsg::ProjectChanged { project } if project.id == p && project.pinned_at.is_some())
    })
    .await;
    let listed: Vec<crew_core::store::StoredProject> = c.ok(Request::ListProjects).await;
    assert!(listed[0].pinned_at.is_some());
    c.call(Request::PinProject {
        project: p,
        pinned: false,
    })
    .await
    .unwrap();
    c.until(|m| {
        matches!(m, ServerMsg::ProjectChanged { project } if project.id == p && project.pinned_at.is_none())
    })
    .await;
    c.call(Request::RemoveProject { project: p }).await.unwrap();
    let err = c
        .call(Request::PinProject {
            project: p,
            pinned: true,
        })
        .await
        .unwrap_err();
    assert_eq!(err, crew_core::store::REMOVED);
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_summaries_cover_every_project_or_one() {
    let bins = tempfile::tempdir().unwrap();
    let d = Daemon::start(writing_claude(bins.path())).await;
    let root = d.path("demo");
    repo(&root, &["src/lib.rs"]);
    let shop_root = d.path("shop");
    std::fs::create_dir_all(&shop_root).unwrap();

    let mut c = d.client().await;
    let demo = c.project("demo", &root).await;
    let shop = c.project("shop", &shop_root).await;
    let general = c.channel(demo, "general").await;
    let bugs = c.channel(demo, "bugs").await;
    let shop_general = c.channel(shop, "general").await;
    let worked = c.thread(general, "Worked").await;
    let archived = c.thread(bugs, "Archived").await;
    let idle = c.thread(shop_general, "Idle").await;
    c.agent("Forge", PermissionMode::Auto).await;
    c.call(Request::SubscribeAll).await.unwrap();
    c.post(worked, "@Forge go").await;
    c.run_reaches(worked, RunStatus::Done).await;
    c.call(Request::ArchiveThread {
        thread: archived,
        archived: true,
    })
    .await
    .unwrap();

    let all: Vec<ThreadSummary> = c
        .ok(Request::ThreadSummaries {
            project: None,
            channels: None,
        })
        .await;
    let mut ids: Vec<ThreadId> = all.iter().map(|s| s.thread).collect();
    ids.sort_by_key(|t| t.0);
    let mut want = vec![worked, idle];
    want.sort_by_key(|t| t.0);
    assert_eq!(ids, want, "archived threads are left out");
    let w = all.iter().find(|s| s.thread == worked).unwrap();
    assert_eq!(w.files, 1, "file.txt in the worktree");
    assert!(w.added > 0);
    assert_eq!(w.runs.len(), 1);
    assert_eq!(w.runs[0].status, RunStatus::Done);

    let one: Vec<ThreadSummary> = c
        .ok(Request::ThreadSummaries {
            project: Some(shop),
            channels: None,
        })
        .await;
    let ids: Vec<ThreadId> = one.iter().map(|s| s.thread).collect();
    assert_eq!(ids, [idle]);
    // Only the channels asked for.
    let some: Vec<ThreadSummary> = c
        .ok(Request::ThreadSummaries {
            project: None,
            channels: Some(vec![general, shop_general]),
        })
        .await;
    let mut ids: Vec<ThreadId> = some.iter().map(|s| s.thread).collect();
    ids.sort_by_key(|t| t.0);
    assert_eq!(ids, want);
    let none: Vec<ThreadSummary> = c
        .ok(Request::ThreadSummaries {
            project: None,
            channels: Some(vec![bugs]),
        })
        .await;
    assert!(none.is_empty(), "only the archived thread is there");

    c.call(Request::RemoveProject { project: shop })
        .await
        .unwrap();
    let after: Vec<ThreadSummary> = c
        .ok(Request::ThreadSummaries {
            project: None,
            channels: None,
        })
        .await;
    assert_eq!(after.iter().map(|s| s.thread).collect::<Vec<_>>(), [worked]);
}
