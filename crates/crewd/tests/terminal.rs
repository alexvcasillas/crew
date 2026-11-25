//! Terminal sessions: Claude Code sessions people run themselves, read
//! from a synthetic claude home. Never the real one.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::claude::transcript::TranscriptItem;
use crew_core::event::{Event, RunStatus};
use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_core::store::StoredRun;
use crew_proto::{
    ClientMsg, PROTO_VERSION, Request, ServerMsg, TerminalSession, read_frame, write_frame,
};
use crewd::{ServeOptions, serve};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::{Instant, timeout};

/// Generous: CI runners are slow, and liveness is polled every 5 s.
const WAIT: Duration = Duration::from_secs(30);

/// Where a test's files live. The project root holds the crew home, so a
/// session in a thread worktree is inside the project too.
struct Paths {
    _tmp: TempDir,
    root: PathBuf,
    /// A link to `crew_data`, so worktree paths differ by spelling.
    crew_home: PathBuf,
    crew_data: PathBuf,
    claude_home: PathBuf,
    other: PathBuf,
}

impl Paths {
    fn new() -> Paths {
        let tmp = tempfile::tempdir().unwrap();
        // Claude Code writes the real path, as `getcwd` gives it.
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("ws");
        let other = base.join("elsewhere");
        let claude_home = base.join("claude");
        let crew_data = root.join(".crew-data");
        for dir in [&root, &other, &claude_home.join("projects"), &crew_data] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let crew_home = base.join("crew-link");
        std::os::unix::fs::symlink(&crew_data, &crew_home).unwrap();
        Paths {
            _tmp: tmp,
            crew_home,
            crew_data,
            root,
            claude_home,
            other,
        }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.claude_home.join("sessions")
    }

    /// The file Claude Code writes for a session started in `cwd`.
    fn transcript(&self, cwd: &Path, id: &str) -> PathBuf {
        let folder: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = self.claude_home.join("projects").join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{id}.jsonl"))
    }
}

fn ts(second: u32) -> String {
    format!("2026-11-20T10:{:02}:{:02}.000Z", second / 60, second % 60)
}

fn user(id: &str, cwd: &Path, entry: &str, second: u32, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "uuid": format!("u{second}"),
        "sessionId": id,
        "timestamp": ts(second),
        "cwd": cwd,
        "entrypoint": entry,
        "isSidechain": false,
    })
    .to_string()
}

fn reply(id: &str, cwd: &Path, second: u32, text: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "message": {
            "id": format!("msg_{second}"),
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
        },
        "uuid": format!("a{second}"),
        "sessionId": id,
        "timestamp": ts(second),
        "cwd": cwd,
        "entrypoint": "cli",
        "isSidechain": false,
    })
    .to_string()
}

fn meta(id: &str) -> String {
    serde_json::json!({"type": "permission-mode", "permissionMode": "default", "sessionId": id})
        .to_string()
}

/// A short terminal session: a prompt and a reply.
fn write_session(path: &Path, id: &str, cwd: &Path, entry: &str, prompt: &str) {
    let lines = [
        meta(id),
        user(id, cwd, entry, 1, prompt),
        reply(id, cwd, 2, "On it."),
    ];
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn mtime_ms(path: &Path) -> i64 {
    let t = std::fs::metadata(path).unwrap().modified().unwrap();
    t.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn set_age(path: &Path, age: Duration) {
    let f = OpenOptions::new().append(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - age).unwrap();
}

fn append(path: &Path, lines: &[String]) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
}

/// Claude Code's registry entry of a running CLI.
fn register(p: &Paths, pid: u32, id: &str, status: &str) -> PathBuf {
    std::fs::create_dir_all(p.sessions_dir()).unwrap();
    let path = p.sessions_dir().join(format!("{pid}.json"));
    let entry = serde_json::json!({
        "pid": pid,
        "sessionId": id,
        "cwd": p.root,
        "kind": "interactive",
        "entrypoint": "cli",
        "status": status,
    });
    std::fs::write(&path, entry.to_string()).unwrap();
    path
}

/// A pid no process has any more.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

async fn start(p: &Paths, claude_bin: PathBuf) {
    let home = p.crew_home.clone();
    let opts = ServeOptions {
        no_title_runs: true,
        claude_bin: Some(claude_bin),
        codex_bin: Some(PathBuf::from("/nonexistent/codex")),
        crew_mcp_bin: None,
        claude_home: Some(p.claude_home.clone()),
        ..Default::default()
    };
    tokio::spawn(async move {
        if let Err(e) = serve(&home, opts).await {
            panic!("serve failed: {e:#}");
        }
    });
    let sock = p.crew_home.join("crewd.sock");
    timeout(WAIT, async {
        while UnixStream::connect(&sock).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("crewd did not start");
}

struct Client {
    stream: UnixStream,
    next_id: u64,
    pending: Vec<ServerMsg>,
}

impl Client {
    async fn connect(p: &Paths) -> Client {
        let mut c = Client {
            stream: UnixStream::connect(p.crew_home.join("crewd.sock"))
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

    /// Messages up to and including the first `done` one.
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

    /// The terminal sessions of a fresh `SubscribeAll` snapshot.
    async fn snapshot(&mut self) -> Vec<TerminalSession> {
        self.call(Request::SubscribeAll).await.unwrap();
        match self
            .until(|m| matches!(m, ServerMsg::Snapshot { .. }))
            .await
            .pop()
        {
            Some(ServerMsg::Snapshot {
                terminal_sessions, ..
            }) => terminal_sessions,
            _ => unreachable!(),
        }
    }

    /// Waits for a change to `id` that `ok` accepts.
    async fn change(&mut self, id: &str, ok: impl Fn(&TerminalSession) -> bool) -> TerminalSession {
        let seen = self
            .until(|m| {
                matches!(m, ServerMsg::TerminalSessionChanged { session }
                    if session.id == id && ok(session))
            })
            .await;
        match seen.last() {
            Some(ServerMsg::TerminalSessionChanged { session }) => session.clone(),
            _ => unreachable!(),
        }
    }
}

async fn create_project(c: &mut Client, root: &Path) -> ProjectId {
    c.ok(Request::CreateProject {
        name: "demo".into(),
        root: root.to_string_lossy().into_owned(),
        main_branch: "main".into(),
        work_mode: None,
        copy_files: vec![],
        setup_command: None,
    })
    .await
}

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

/// A claude that replays a short stream-json run.
fn replaying_claude(dir: &Path) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    script(
        &dir.join("claude-replay.sh"),
        &format!(
            "exec '{}' '{}'\n",
            manifest.join("tests/fake/claude_ok.sh").display(),
            manifest
                .join("../crew-core/tests/fixtures/claude/read_file.jsonl")
                .display(),
        ),
    )
}

fn no_claude() -> PathBuf {
    PathBuf::from("/nonexistent/claude")
}

/// Runs one Crew agent in the project and returns its session id.
async fn crew_run_session(c: &mut Client, project: ProjectId) -> String {
    let channel: ChannelId = c
        .ok(Request::CreateChannel {
            project,
            name: "general".into(),
        })
        .await;
    let spec = AgentSpec {
        name: "Forge".into(),
        role: "Builds things".into(),
        runtime: Runtime::ClaudeCode,
        model: "claude-haiku-4-5".into(),
        instructions: String::new(),
        permission_mode: PermissionMode::ReadOnly,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    };
    let _: AgentId = c.ok(Request::CreateAgent { spec }).await;
    let thread: ThreadId = c
        .ok(Request::CreateThread {
            channel,
            title: "t".into(),
        })
        .await;
    c.call(Request::Subscribe { thread }).await.unwrap();
    c.call(Request::PostMessage {
        thread,
        body: "@Forge read it".into(),
        skill: None,
        model_override: None,
    })
    .await
    .unwrap();
    c.until(|m| {
        matches!(
            m,
            ServerMsg::RunStatus {
                status: RunStatus::Done,
                ..
            }
        )
    })
    .await;
    let runs: Vec<StoredRun> = c.ok(Request::ListRuns { status: None }).await;
    assert_eq!(runs.len(), 1);
    assert!(!runs[0].session_id.is_empty());
    runs[0].session_id.clone()
}

const SHOWN: &str = "00000000-0000-4000-8000-00000000000a";

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_session_shows_under_its_project() {
    let p = Paths::new();
    let sub = p.root.join("crates/app");
    std::fs::create_dir_all(&sub).unwrap();
    // Written before crewd starts and before the project exists.
    write_session(
        &p.transcript(&sub, SHOWN),
        SHOWN,
        &sub,
        "cli",
        "Fix the login\u{1b}[0m bug\nand add a test",
    );
    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    assert!(c.snapshot().await.is_empty());

    let project = create_project(&mut c, &p.root).await;
    let session = c.change(SHOWN, |_| true).await;
    assert_eq!(session.project, project);
    assert_eq!(session.cwd, sub.to_string_lossy());
    assert_eq!(session.title, "Fix the login bug");
    // 2026-11-20T10:00:01Z, the first line's time.
    assert_eq!(session.started_at, 1_795_168_801_000);
    assert_eq!(session.last_active_at, mtime_ms(&p.transcript(&sub, SHOWN)));
    // No registry and a fresh file: live by its age.
    assert!(session.live);

    let mut other = Client::connect(&p).await;
    assert_eq!(other.snapshot().await, vec![session]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_outside_projects_in_worktrees_or_of_crew_runs_are_ignored() {
    let p = Paths::new();
    let tmp = tempfile::tempdir().unwrap();
    start(&p, replaying_claude(tmp.path())).await;
    let mut c = Client::connect(&p).await;
    let project = create_project(&mut c, &p.root).await;
    let run_session = crew_run_session(&mut c, project).await;
    c.snapshot().await;

    let ignored = [
        (
            "00000000-0000-4000-8000-0000000000b1",
            p.other.clone(),
            "cli",
        ),
        (
            "00000000-0000-4000-8000-0000000000b2",
            // Where the linked crew home really is, as Claude Code
            // writes it.
            p.crew_data.join("worktrees/demo/fix-login"),
            "cli",
        ),
        // A name that only starts like the root's.
        (
            "00000000-0000-4000-8000-0000000000b3",
            p.root.with_file_name("ws2"),
            "cli",
        ),
        (
            "00000000-0000-4000-8000-0000000000b4",
            p.root.clone(),
            "sdk-cli",
        ),
        (run_session.as_str(), p.root.clone(), "cli"),
    ];
    for (id, cwd, entry) in &ignored {
        std::fs::create_dir_all(cwd).unwrap();
        write_session(&p.transcript(cwd, id), id, cwd, entry, "hidden");
    }
    write_session(
        &p.transcript(&p.root, SHOWN),
        SHOWN,
        &p.root,
        "cli",
        "shown",
    );

    let seen = c
        .until(
            |m| matches!(m, ServerMsg::TerminalSessionChanged { session } if session.id == SHOWN),
        )
        .await;
    for m in &seen {
        if let ServerMsg::TerminalSessionChanged { session } = m {
            assert_eq!(session.id, SHOWN, "{session:?} should be ignored");
        }
    }
    let mut other = Client::connect(&p).await;
    let ids: Vec<String> = other.snapshot().await.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![SHOWN.to_string()]);
}

fn terminal_items(msgs: &[ServerMsg], id: &str) -> Vec<(u64, TranscriptItem)> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerMsg::TerminalEvent { session, seq, item } if session == id => {
                Some((*seq, item.clone()))
            }
            _ => None,
        })
        .collect()
}

fn says(item: &TranscriptItem, text: &str) -> bool {
    match item {
        TranscriptItem::UserPrompt(t) => t == text,
        TranscriptItem::Event(Event::Message { text: t }) => t == text,
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn appended_lines_stream_live_to_subscribers() {
    let p = Paths::new();
    let path = p.transcript(&p.root, SHOWN);
    write_session(&path, SHOWN, &p.root, "cli", "First ask");
    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    // Subscribed first: a session found once the project exists is then
    // always told as a change, never only in the snapshot.
    c.snapshot().await;
    create_project(&mut c, &p.root).await;
    c.change(SHOWN, |_| true).await;

    let err = c
        .call(Request::SubscribeTerminal {
            session: "no-such-session".into(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("terminal session"), "{err}");

    c.call(Request::SubscribeTerminal {
        session: SHOWN.into(),
    })
    .await
    .unwrap();
    let backlog = c
        .until(|m| matches!(m, ServerMsg::TerminalEvent { item, .. } if says(item, "On it.")))
        .await;
    let items = terminal_items(&backlog, SHOWN);
    assert_eq!(items.len(), 2);
    assert!(says(&items[0].1, "First ask"));
    assert!(items[0].0 < items[1].0);

    // Written in two steps: half a line waits for the rest.
    append(&path, &[user(SHOWN, &p.root, "cli", 3, "Second ask")]);
    let tail = reply(SHOWN, &p.root, 4, "Done.");
    let (head, rest) = tail.split_at(tail.len() / 2);
    {
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(head.as_bytes()).unwrap();
    }
    let live = c
        .until(|m| matches!(m, ServerMsg::TerminalEvent { item, .. } if says(item, "Second ask")))
        .await;
    let got = terminal_items(&live, SHOWN);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, items[1].0 + 1);
    append(&path, &[rest.to_string()]);
    let live = c
        .until(|m| matches!(m, ServerMsg::TerminalEvent { item, .. } if says(item, "Done.")))
        .await;
    let got = terminal_items(&live, SHOWN);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, items[1].0 + 2);

    // A second subscriber starts from the same backlog, seqs included.
    let mut other = Client::connect(&p).await;
    other
        .call(Request::SubscribeTerminal {
            session: SHOWN.into(),
        })
        .await
        .unwrap();
    let again = other
        .until(|m| matches!(m, ServerMsg::TerminalEvent { item, .. } if says(item, "Done.")))
        .await;
    let again = terminal_items(&again, SHOWN);
    assert_eq!(again.len(), 4);
    assert_eq!(again[0].0, items[0].0);
    assert_eq!(again[3].0, items[1].0 + 2);

    // After unsubscribing, the session still changes but sends no items.
    c.call(Request::UnsubscribeTerminal {
        session: SHOWN.into(),
    })
    .await
    .unwrap();
    append(&path, &[reply(SHOWN, &p.root, 9, "Quiet.")]);
    let written = mtime_ms(&path);
    let seen = c
        .until(|m| {
            matches!(m, ServerMsg::TerminalSessionChanged { session }
            if session.last_active_at >= written)
        })
        .await;
    assert!(terminal_items(&seen, SHOWN).is_empty(), "{seen:?}");
    // The other subscriber still gets it.
    other
        .until(|m| matches!(m, ServerMsg::TerminalEvent { item, .. } if says(item, "Quiet.")))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn liveness_comes_from_the_session_registry() {
    let p = Paths::new();
    let busy = "00000000-0000-4000-8000-0000000000c1";
    let gone = "00000000-0000-4000-8000-0000000000c2";
    let unlisted = "00000000-0000-4000-8000-0000000000c3";
    for id in [busy, gone, unlisted] {
        write_session(&p.transcript(&p.root, id), id, &p.root, "cli", id);
    }
    let entry = register(&p, std::process::id(), busy, "busy");
    register(&p, dead_pid(), gone, "busy");

    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    // Subscribed first: a session found once the project exists is then
    // always told as a change, never only in the snapshot.
    c.snapshot().await;
    create_project(&mut c, &p.root).await;
    let deadline = Instant::now() + WAIT;
    let mut live = Vec::new();
    while Instant::now() < deadline {
        let mut s = Client::connect(&p).await;
        let sessions = s.snapshot().await;
        if sessions.len() == 3 {
            live = sessions
                .iter()
                .filter(|s| s.live)
                .map(|s| s.id.clone())
                .collect();
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(live, vec![busy.to_string()]);

    // Idle at its prompt: not working.
    register(&p, std::process::id(), busy, "shell");
    c.change(busy, |s| !s.live).await;
    register(&p, std::process::id(), busy, "busy");
    c.change(busy, |s| s.live).await;
    // The CLI ended and took its entry along.
    std::fs::remove_file(&entry).unwrap();
    c.change(busy, |s| !s.live).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_registry_old_files_are_not_live() {
    let p = Paths::new();
    let old = "00000000-0000-4000-8000-0000000000d1";
    let path = p.transcript(&p.root, old);
    write_session(&path, old, &p.root, "cli", "old");
    set_age(&path, Duration::from_secs(600));

    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    // Subscribed first: a session found once the project exists is then
    // always told as a change, never only in the snapshot.
    c.snapshot().await;
    create_project(&mut c, &p.root).await;
    let session = c.change(old, |_| true).await;
    assert!(!session.live);
    // Written to again: live.
    append(&path, &[reply(old, &p.root, 5, "back")]);
    c.change(old, |s| s.live).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_file_is_only_tailed() {
    let p = Paths::new();
    let path = p.transcript(&p.root, SHOWN);
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        writeln!(f, "{}", user(SHOWN, &p.root, "cli", 1, "First ask")).unwrap();
        let filler = "x".repeat(2000);
        let mut n = 0;
        while f.get_ref().metadata().unwrap().len() < 50 * 1024 * 1024 {
            for _ in 0..1000 {
                writeln!(f, "{}", reply(SHOWN, &p.root, 2, &format!("{n} {filler}"))).unwrap();
                n += 1;
            }
            f.flush().unwrap();
        }
        writeln!(f, "{}", reply(SHOWN, &p.root, 3, "Last words")).unwrap();
    }

    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    // Subscribed first: a session found once the project exists is then
    // always told as a change, never only in the snapshot.
    c.snapshot().await;
    create_project(&mut c, &p.root).await;
    let session = c.change(SHOWN, |_| true).await;
    // The title comes from the start of the file.
    assert_eq!(session.title, "First ask");

    c.call(Request::SubscribeTerminal {
        session: SHOWN.into(),
    })
    .await
    .unwrap();
    let backlog = c
        .until(|m| matches!(m, ServerMsg::TerminalEvent { item, .. } if says(item, "Last words")))
        .await;
    let items = terminal_items(&backlog, SHOWN);
    assert_eq!(items.len(), 2000);
    assert!(items.windows(2).all(|w| w[1].0 == w[0].0 + 1));
    assert!(!items.iter().any(|(_, i)| says(i, "First ask")));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_found_by_polling_when_the_folder_comes_later() {
    let p = Paths::new();
    // No projects folder to watch yet.
    std::fs::remove_dir(p.claude_home.join("projects")).unwrap();
    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    // Subscribed first: a session found once the project exists is then
    // always told as a change, never only in the snapshot.
    c.snapshot().await;
    create_project(&mut c, &p.root).await;

    write_session(&p.transcript(&p.root, SHOWN), SHOWN, &p.root, "cli", "Late");
    let session = c.change(SHOWN, |_| true).await;
    assert_eq!(session.title, "Late");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewritten_file_resets_its_backlog() {
    let p = Paths::new();
    let path = p.transcript(&p.root, SHOWN);
    write_session(&path, SHOWN, &p.root, "cli", "Old ask");
    append(&path, &[reply(SHOWN, &p.root, 3, "Old answer")]);
    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    // Subscribed first: a session found once the project exists is then
    // always told as a change, never only in the snapshot.
    c.snapshot().await;
    create_project(&mut c, &p.root).await;
    c.change(SHOWN, |_| true).await;
    c.call(Request::SubscribeTerminal {
        session: SHOWN.into(),
    })
    .await
    .unwrap();
    let old = c
        .until(|m| matches!(m, ServerMsg::TerminalEvent { item, .. } if says(item, "Old answer")))
        .await;
    let last_old = terminal_items(&old, SHOWN).last().unwrap().0;

    // Cut short and written anew, as a rotated file would be.
    std::fs::write(&path, user(SHOWN, &p.root, "cli", 1, "New ask") + "\n").unwrap();
    let seen = c
        .until(|m| matches!(m, ServerMsg::TerminalSessionReset { session } if session == SHOWN))
        .await;
    // Not sent as new items.
    assert!(terminal_items(&seen, SHOWN).is_empty(), "{seen:?}");
    c.call(Request::SubscribeTerminal {
        session: SHOWN.into(),
    })
    .await
    .unwrap();
    let fresh = c
        .until(|m| matches!(m, ServerMsg::TerminalEvent { item, .. } if says(item, "New ask")))
        .await;
    let fresh = terminal_items(&fresh, SHOWN);
    assert_eq!(fresh.len(), 1, "{fresh:?}");
    assert!(fresh[0].0 > last_old);
}

#[tokio::test(flavor = "multi_thread")]
async fn old_and_deleted_sessions_are_not_listed() {
    let p = Paths::new();
    let old = "00000000-0000-4000-8000-0000000000e1";
    let old_path = p.transcript(&p.root, old);
    write_session(&old_path, old, &p.root, "cli", "long ago");
    set_age(&old_path, Duration::from_secs(15 * 24 * 3600));
    let path = p.transcript(&p.root, SHOWN);
    write_session(&path, SHOWN, &p.root, "cli", "recent");

    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    // Subscribed first: a session found once the project exists is then
    // always told as a change, never only in the snapshot.
    c.snapshot().await;
    create_project(&mut c, &p.root).await;
    c.change(SHOWN, |_| true).await;
    let mut other = Client::connect(&p).await;
    let ids: Vec<String> = other.snapshot().await.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![SHOWN.to_string()]);

    std::fs::remove_file(&path).unwrap();
    c.until(|m| matches!(m, ServerMsg::TerminalSessionRemoved { id } if id == SHOWN))
        .await;
    let mut other = Client::connect(&p).await;
    assert!(other.snapshot().await.is_empty());
    let err = c
        .call(Request::SubscribeTerminal {
            session: SHOWN.into(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("terminal session"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_project_takes_its_terminal_sessions_along() {
    let p = Paths::new();
    write_session(
        &p.transcript(&p.root, SHOWN),
        SHOWN,
        &p.root,
        "cli",
        "Fix the login bug",
    );
    start(&p, no_claude()).await;
    let mut c = Client::connect(&p).await;
    c.snapshot().await;
    let project = create_project(&mut c, &p.root).await;
    c.change(SHOWN, |_| true).await;

    c.call(Request::RemoveProject { project }).await.unwrap();
    c.until(|m| matches!(m, ServerMsg::TerminalSessionRemoved { id } if id == SHOWN))
        .await;
    // A rescan doesn't bring it back, and the session file stays.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut other = Client::connect(&p).await;
    assert!(other.snapshot().await.is_empty());
    assert!(p.transcript(&p.root, SHOWN).exists());
}
