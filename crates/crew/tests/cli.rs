use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use crewd::{ServeOptions, serve};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A fake claude that replays the edit_and_test fixture.
fn replay_script(dir: &Path) -> PathBuf {
    let fake = manifest_dir().join("../crewd/tests/fake/claude_ok.sh");
    let fixture = manifest_dir().join("../crew-core/tests/fixtures/claude/edit_and_test.jsonl");
    let path = dir.join("claude-replay.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec '{}' '{}'\n",
            fake.display(),
            fixture.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Runs crewd on its own runtime in a background thread until the test ends.
fn start_crewd(home: &Path, claude_bin: PathBuf) {
    start_crewd_with(home, claude_bin, None);
}

/// Like [`start_crewd`], with `codex_bin` as `codex`.
fn start_crewd_with(home: &Path, claude_bin: PathBuf, codex_bin: Option<PathBuf>) {
    let serve_home = home.to_path_buf();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let opts = ServeOptions {
                no_title_runs: true,
                max_runs: 4,
                claude_bin: Some(claude_bin),
                // Never the real codex.
                codex_bin: Some(codex_bin.unwrap_or_else(|| PathBuf::from("/nonexistent/codex"))),
                crew_mcp_bin: None,
                ..Default::default()
            };
            serve(&serve_home, opts).await.unwrap();
        });
    });
    for _ in 0..500 {
        if std::os::unix::net::UnixStream::connect(home.join("crewd.sock")).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("crewd did not start");
}

fn crew(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("crew").unwrap();
    cmd.env("CREW_HOME", home)
        .current_dir(cwd)
        .timeout(Duration::from_secs(20));
    cmd
}

fn stdout_of(cmd: &mut Command) -> String {
    let out = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

#[test]
fn say_then_watch_prints_events_and_reply() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir(&home).unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    start_crewd(&home, replay_script(tmp.path()));

    let thread = stdout_of(crew(&home, &project).arg("init-demo"))
        .trim()
        .to_string();
    assert_eq!(
        thread.len(),
        36,
        "init-demo should print only the thread id"
    );

    // Running init-demo again reuses the agent.
    stdout_of(crew(&home, &project).arg("init-demo"));

    stdout_of(crew(&home, &project).args(["say", &thread, "@Scout", "fix", "it"]));
    let out = stdout_of(crew(&home, &project).args(["watch", &thread, "--until-idle"]));
    let lines: Vec<&str> = out.lines().collect();
    for want in [
        "you: @Scout fix it",
        "[Scout] started (claude-haiku-4-5)",
        "[Scout] edit /work/demo/src/lib.rs",
        "[Scout] run cargo test",
        "[Scout] ✓ done",
        "Scout: Fixed.",
    ] {
        assert!(lines.contains(&want), "missing {want:?} in:\n{out}");
    }
    let edit = lines
        .iter()
        .position(|l| l.starts_with("[Scout] edit"))
        .unwrap();
    let run = lines
        .iter()
        .position(|l| l.starts_with("[Scout] run"))
        .unwrap();
    assert!(edit < run);

    let status = stdout_of(crew(&home, &project).arg("status"));
    assert_eq!(status.trim(), "Nothing running.");
}

#[test]
fn status_and_stop_all_with_a_sleeping_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir(&home).unwrap();
    let sleeps = manifest_dir().join("../crewd/tests/fake/claude_sleeps.sh");
    start_crewd(&home, sleeps);

    let thread = stdout_of(crew(&home, tmp.path()).arg("init-demo"))
        .trim()
        .to_string();
    stdout_of(crew(&home, tmp.path()).args(["say", &thread, "@Scout wait"]));

    let mut line = String::new();
    for _ in 0..100 {
        line = stdout_of(crew(&home, tmp.path()).arg("status"));
        if line.contains("Working") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let cols: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(cols.len(), 4, "got {line:?}");
    assert_eq!(cols[1], "Scout");
    assert_eq!(cols[2], "Working");
    assert_eq!(cols[3], &thread[..8]);

    stdout_of(crew(&home, tmp.path()).args(["stop", "--all"]));
    let out = stdout_of(crew(&home, tmp.path()).args(["watch", &thread, "--until-idle"]));
    assert!(out.contains("[Scout] ■ stopped"), "got:\n{out}");
    assert!(out.contains("you: @Scout wait"), "got:\n{out}");
}

#[test]
fn missing_daemon_is_explained() {
    let tmp = tempfile::tempdir().unwrap();
    crew(tmp.path(), tmp.path())
        .arg("status")
        .assert()
        .code(1)
        .stderr("crewd isn't running. Start it with `crewd`.\n");
}

/// Starts a sleeping run and returns (thread id, short run id).
fn start_sleeping_run(home: &Path, cwd: &Path) -> (String, String) {
    let thread = stdout_of(crew(home, cwd).arg("init-demo"))
        .trim()
        .to_string();
    stdout_of(crew(home, cwd).args(["say", &thread, "@Scout wait"]));
    for _ in 0..100 {
        let line = stdout_of(crew(home, cwd).arg("status"));
        if line.contains("Working") {
            let run8 = line.split_whitespace().next().unwrap().to_string();
            return (thread, run8);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("run never started");
}

#[test]
fn stop_accepts_the_short_run_id_from_status() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir(&home).unwrap();
    start_crewd(
        &home,
        manifest_dir().join("../crewd/tests/fake/claude_sleeps.sh"),
    );
    let (thread, run8) = start_sleeping_run(&home, tmp.path());
    assert_eq!(run8.len(), 8);

    stdout_of(crew(&home, tmp.path()).args(["stop", &run8]));
    let out = stdout_of(crew(&home, tmp.path()).args(["watch", &thread, "--until-idle"]));
    assert!(out.contains("[Scout] ■ stopped"), "got:\n{out}");
    assert!(out.contains("system: Scout stopped."), "got:\n{out}");
}

#[test]
fn stop_thread_accepts_a_short_thread_id() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir(&home).unwrap();
    start_crewd(
        &home,
        manifest_dir().join("../crewd/tests/fake/claude_sleeps.sh"),
    );
    let (thread, _) = start_sleeping_run(&home, tmp.path());

    stdout_of(crew(&home, tmp.path()).args(["stop", "--thread", &thread[..8]]));
    let out = stdout_of(crew(&home, tmp.path()).args(["watch", &thread[..8], "--until-idle"]));
    assert!(out.contains("[Scout] ■ stopped"), "got:\n{out}");
}

#[test]
fn unknown_and_too_short_ids_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir(&home).unwrap();
    start_crewd(
        &home,
        manifest_dir().join("../crewd/tests/fake/claude_sleeps.sh"),
    );
    for (args, want) in [
        (["stop", "ab"], "not a valid run id"),
        (["watch", "ffffffff"], "Use the full thread id."),
    ] {
        let out = crew(&home, tmp.path()).args(args).assert().code(1);
        let err = String::from_utf8(out.get_output().stderr.clone()).unwrap();
        assert!(err.contains(want), "got: {err}");
    }
}

#[test]
fn status_lists_queued_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir(&home).unwrap();
    start_crewd(
        &home,
        manifest_dir().join("../crewd/tests/fake/claude_sleeps.sh"),
    );
    let (thread, _) = start_sleeping_run(&home, tmp.path());
    // One run per thread: the second one waits.
    stdout_of(crew(&home, tmp.path()).args(["say", &thread, "@Scout next"]));

    let status = stdout_of(crew(&home, tmp.path()).arg("status"));
    let states: Vec<&str> = status
        .lines()
        .map(|l| l.split_whitespace().nth(2).unwrap_or(""))
        .collect();
    assert_eq!(states, vec!["Working", "Queued"], "got:\n{status}");

    stdout_of(crew(&home, tmp.path()).args(["stop", "--all"]));
    let started = std::time::Instant::now();
    let out = stdout_of(crew(&home, tmp.path()).args(["watch", &thread, "--until-idle"]));
    assert!(
        out.contains("system: Stopped before starting."),
        "got:\n{out}"
    );
    // A run that never started gets no "stopped" message to wait for.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "watch waited {:?} for a message that never comes",
        started.elapsed()
    );
}

/// A fake claude for handoff tests. Its first run writes `file.txt` and
/// replies with a mention of Sentinel, every later run replies "Looks fine."
fn handoff_script(dir: &Path) -> PathBuf {
    let path = dir.join("claude-handoff.sh");
    let script = format!(
        r#"#!/bin/sh
n=$(cat '{dir}/n' 2>/dev/null || echo 0)
n=$((n + 1))
echo $n > '{dir}/n'
if [ "$n" = 1 ]; then
  printf 'new\n' > file.txt
  reply='@Sentinel please check'
else
  reply='Looks fine.'
fi
printf '{{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":[]}}\n'
printf '{{"type":"result","subtype":"success","is_error":false,"result":"%s","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}}\n' "$reply"
"#,
        dir = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A crew home, a git project with a demo thread, and agents Forge (edits)
/// and Sentinel (read-only).
struct Setup {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    thread: String,
}

fn setup() -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir(&home).unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["config", "user.name", "Test"]);
    git(&project, &["config", "user.email", "test@example.com"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    std::fs::write(project.join("a.txt"), "a\n").unwrap();
    git(&project, &["add", "a.txt"]);
    git(&project, &["commit", "-q", "-m", "init"]);
    start_crewd(&home, handoff_script(tmp.path()));

    let thread = stdout_of(crew(&home, &project).arg("init-demo"))
        .trim()
        .to_string();
    stdout_of(crew(&home, &project).args([
        "agent", "add", "Forge", "--model", "haiku", "--role", "Builds", "--edit",
    ]));
    stdout_of(crew(&home, &project).args([
        "agent", "add", "Sentinel", "--model", "haiku", "--role", "Reviews",
    ]));
    Setup {
        _tmp: tmp,
        home,
        project,
        thread,
    }
}

impl Setup {
    fn crew(&self) -> Command {
        crew(&self.home, &self.project)
    }

    /// Asks Forge to work and waits until its handoff to Sentinel is pending.
    /// Returns the approval's short id.
    fn forge_asks(&self) -> String {
        stdout_of(self.crew().args(["say", &self.thread, "@Forge", "go"]));
        let out =
            stdout_of(
                self.crew()
                    .args(["watch", &self.thread, "--until-idle", "--until-ask"]),
            );
        let line = out
            .lines()
            .find(|l| l.starts_with("[?] "))
            .unwrap_or_else(|| panic!("no approval line in:\n{out}"));
        assert!(
            line.starts_with("[?] Forge wants to bring in Sentinel.  (crew approve "),
            "got {line:?}"
        );
        line.trim_end_matches(')')
            .rsplit(' ')
            .next()
            .unwrap()
            .to_string()
    }
}

/// Sends one request straight to crewd and decodes the answer.
fn ask<T: serde::de::DeserializeOwned>(home: &Path, req: crew_proto::Request) -> T {
    use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg};
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut stream = tokio::net::UnixStream::connect(home.join("crewd.sock"))
            .await
            .unwrap();
        for (id, req) in [
            (
                1,
                Request::Hello {
                    version: PROTO_VERSION,
                },
            ),
            (2, req),
        ] {
            crew_proto::write_frame(&mut stream, &ClientMsg::Request { id, req })
                .await
                .unwrap();
        }
        loop {
            match crew_proto::read_frame::<_, ServerMsg>(&mut stream)
                .await
                .unwrap()
            {
                Some(ServerMsg::Response { id: 2, result }) => {
                    return serde_json::from_value(result.unwrap()).unwrap();
                }
                Some(_) => {}
                None => panic!("crewd hung up"),
            }
        }
    })
}

#[test]
fn agent_add_defaults_to_read_only() {
    let s = setup();
    let agents: Vec<crew_core::store::StoredAgent> = ask(
        &s.home,
        crew_proto::Request::ListAgents {
            include_deleted: false,
        },
    );
    use crew_core::agent::{PermissionMode, Runtime};
    let spec = |name: &str| &agents.iter().find(|a| a.spec.name == name).unwrap().spec;
    assert_eq!(spec("Sentinel").permission_mode, PermissionMode::ReadOnly);
    assert_eq!(spec("Sentinel").runtime, Runtime::ClaudeCode);
    assert_eq!(spec("Sentinel").max_run_minutes, 0, "no limit");
    assert_eq!(spec("Forge").permission_mode, PermissionMode::EditFiles);
}

#[test]
fn approve_runs_the_handoff_and_watch_shows_the_decision() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let s = setup();
    stdout_of(s.crew().args(["say", &s.thread, "@Forge", "go"]));

    /// Kills the child when the test ends, however it ends.
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Kill(
        std::process::Command::new(assert_cmd::cargo::cargo_bin("crew"))
            .env("CREW_HOME", &s.home)
            .args(["watch", &s.thread, "--until-idle"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    // Read on a thread so the wait for the lines can time out.
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.0.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let next = |rx: &std::sync::mpsc::Receiver<String>| {
        rx.recv_timeout(Duration::from_secs(20))
            .expect("watch printed nothing in time")
    };
    let mut seen = Vec::new();
    let id8 = loop {
        let line = next(&rx);
        seen.push(line.clone());
        if let Some(rest) = line.strip_prefix("[?] ") {
            assert!(rest.starts_with("Forge wants to bring in Sentinel.  (crew approve "));
            break rest
                .trim_end_matches(')')
                .rsplit(' ')
                .next()
                .unwrap()
                .to_string();
        }
    };
    assert_eq!(id8.len(), 8);

    let listed = stdout_of(s.crew().arg("approvals"));
    assert!(
        listed.starts_with(&format!("{id8}  {}", &s.thread[..8])),
        "got {listed:?}"
    );
    assert!(listed.contains("Forge wants to bring in Sentinel."));

    stdout_of(s.crew().args(["approve", &id8]));
    // Watch exits by itself once the thread is idle, which ends the stream.
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(20)) {
        seen.push(line);
    }
    let status = child.0.wait().unwrap();
    let out = seen.join("\n");
    assert!(status.success(), "watch failed:\n{out}");
    for want in [
        "[✓] approved",
        "system: Forge brought in Sentinel.",
        "[Sentinel] ✓ done",
        "Sentinel: Looks fine.",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert_eq!(
        stdout_of(s.crew().arg("approvals")).trim(),
        "Nothing waiting."
    );
}

#[test]
fn decline_drops_the_handoff() {
    let s = setup();
    let id8 = s.forge_asks();
    stdout_of(s.crew().args(["decline", &id8]));
    let out = stdout_of(s.crew().args(["watch", &s.thread, "--until-idle"]));
    assert!(out.contains("system: Declined Sentinel."), "got:\n{out}");
    assert!(!out.contains("[Sentinel]"), "got:\n{out}");
}

#[test]
fn watch_waits_for_a_pending_approval_unless_asked_not_to() {
    let s = setup();
    s.forge_asks();
    // Without --until-ask the thread is not idle, so watch keeps waiting.
    let mut cmd = s.crew();
    let out = cmd
        .args(["watch", &s.thread, "--until-idle"])
        .timeout(Duration::from_secs(3))
        .assert()
        .failure();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("[?] Forge wants to bring in Sentinel."));
}

#[test]
fn diff_commit_and_discard_work_on_the_thread_worktree() {
    let s = setup();
    let ask = s.forge_asks();
    stdout_of(s.crew().args(["decline", &ask]));

    let stat = stdout_of(s.crew().args(["diff", &s.thread, "--stat"]));
    assert_eq!(stat.trim(), "A file.txt +1 -0", "got {stat:?}");
    let full = stdout_of(s.crew().args(["diff", &s.thread]));
    assert!(full.starts_with("A file.txt +1 -0\n\n"), "got {full:?}");
    assert!(full.contains("+new"), "got {full:?}");
    assert!(!s.project.join("file.txt").exists(), "root must stay clean");

    let sha = stdout_of(s.crew().args(["commit", &s.thread, "-m", "Add file"]));
    assert_eq!(sha.trim().len(), 8);
    assert_eq!(
        stdout_of(s.crew().args(["diff", &s.thread])).trim(),
        "No changes."
    );

    // Discard asks for confirmation first.
    let refused = s.crew().args(["discard", &s.thread]).assert().code(1);
    let err = String::from_utf8(refused.get_output().stderr.clone()).unwrap();
    assert!(err.contains("--yes"), "got {err}");
    assert!(git(&s.project, &["branch", "--list", "crew/*"]).contains("crew/"));

    stdout_of(s.crew().args(["discard", &s.thread, "--yes"]));
    // The branch holds a commit that isn't merged, so it stays.
    assert_eq!(
        git(&s.project, &["branch", "--list", "crew/*"]),
        "crew/demo"
    );
    let out = stdout_of(s.crew().args(["watch", &s.thread, "--until-idle"]));
    assert!(
        out.contains("system: Kept branch crew/demo because it has commits that aren't merged."),
        "got {out}"
    );
    assert_eq!(
        stdout_of(s.crew().args(["diff", &s.thread])).trim(),
        "No changes."
    );
}

#[test]
fn allow_more_works_and_unknown_approval_ids_are_refused() {
    let s = setup();
    stdout_of(s.crew().args(["allow-more", &s.thread[..8], "2"]));
    let err = s
        .crew()
        .args(["approve", "abcd"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("No approval matches")
    );
}

#[test]
fn thread_and_channel_handoff_settings_are_saved() {
    use crew_core::policy::HandoffPolicy;
    use crew_core::store::{StoredChannel, StoredProject, StoredThread};
    use crew_proto::Request;
    let s = setup();
    stdout_of(s.crew().args([
        "thread",
        "handoff",
        &s.thread[..8],
        "budget",
        "--budget",
        "2",
    ]));
    let projects: Vec<StoredProject> = ask(&s.home, Request::ListProjects);
    let channels: Vec<StoredChannel> = ask(
        &s.home,
        Request::ListChannels {
            project: projects[0].id,
        },
    );
    let channel = channels[0].id;
    stdout_of(
        s.crew()
            .args(["channel", "handoff", &channel.to_string()[..8], "per-agent"]),
    );

    let threads: Vec<StoredThread> = ask(&s.home, Request::ListThreads { channel });
    let thread = threads
        .iter()
        .find(|t| t.id.to_string() == s.thread)
        .unwrap();
    assert_eq!(thread.handoff_policy, Some(HandoffPolicy::Budget));
    assert_eq!(thread.handoff_budget, Some(2));
    let channels: Vec<StoredChannel> = ask(
        &s.home,
        Request::ListChannels {
            project: projects[0].id,
        },
    );
    assert_eq!(channels[0].handoff_policy, Some(HandoffPolicy::PerAgent));
    assert_eq!(channels[0].handoff_budget, None);
}

#[test]
fn agent_add_checks_budget_and_minutes() {
    let s = setup();
    let add = |extra: &[&str]| {
        let mut cmd = s.crew();
        cmd.args([
            "agent", "add", "Quill", "--model", "haiku", "--role", "Writes",
        ])
        .args(extra);
        cmd
    };
    let stderr = |cmd: &mut Command| {
        String::from_utf8(cmd.assert().failure().get_output().stderr.clone()).unwrap()
    };
    assert!(stderr(&mut add(&["--budget", "2"])).contains("--policy"));
    assert!(
        stderr(&mut add(&["--policy", "ask", "--budget", "2"]))
            .contains("--budget needs --policy budget.")
    );
    assert!(stderr(&mut add(&["--max-minutes", "soon"])).contains("--max-minutes"));
    stdout_of(&mut add(&[
        "--policy",
        "budget",
        "--budget",
        "2",
        "--max-minutes",
        "1",
    ]));
}

/// A signed-in `codex` whose app-server lists two models, Luna second.
fn fake_codex(dir: &Path) -> PathBuf {
    fake_codex_after(dir, 0.0)
}

/// Like [`fake_codex`], listing the models only after `delay` seconds.
fn fake_codex_after(dir: &Path, delay: f64) -> PathBuf {
    let fake = manifest_dir().join("../crew-core/tests/fake/codex_app_server.py");
    let models = dir.join("models.json");
    let steps = serde_json::json!({"steps": [
        {"expect": "initialize", "as": "init"},
        {"respond": "init", "result": {}},
        {"expect": "initialized"},
        {"expect": "model/list", "as": "list"},
        {"sleep": delay},
        {"respond": "list", "result": {"data": [
            {"id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "GPT-6.1-Sol", "hidden": false},
            {"id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna", "hidden": false}
        ], "nextCursor": null}}
    ]});
    std::fs::write(&models, steps.to_string()).unwrap();
    let path = dir.join("codex");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'codex-cli 0.165.0' ;;\nlogin) exit 0 ;;\napp-server) exec python3 '{}' '{}' ;;\n*) exit 2 ;;\nesac\n",
            fake.display(),
            models.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn agent_add_makes_codex_agents_on_the_default_model() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    start_crewd_with(
        &home,
        replay_script(tmp.path()),
        Some(fake_codex(tmp.path())),
    );
    let add = |args: &[&str]| {
        let mut cmd = crew(&home, tmp.path());
        cmd.args(["agent", "add"]).args(args);
        cmd
    };
    let out = stdout_of(&mut add(&[
        "Nova",
        "--runtime",
        "codex",
        "--role",
        "Builds",
    ]));
    assert!(out.contains("Nova uses gpt-6-luna (GPT-6-Luna)."), "{out}");
    stdout_of(&mut add(&[
        "Kit",
        "--runtime",
        "codex",
        "--model",
        "gpt-6.1-sol",
        "--role",
        "Builds",
        "--edit",
    ]));
    let err = add(&["Quill", "--role", "Writes"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("--model"), "{err}");

    let agents: Vec<crew_core::store::StoredAgent> = ask(
        &home,
        crew_proto::Request::ListAgents {
            include_deleted: false,
        },
    );
    use crew_core::agent::{PermissionMode, Runtime};
    let spec = |name: &str| &agents.iter().find(|a| a.spec.name == name).unwrap().spec;
    assert_eq!(spec("Nova").runtime, Runtime::Codex);
    assert_eq!(spec("Nova").model, "gpt-6-luna");
    assert_eq!(spec("Nova").permission_mode, PermissionMode::ReadOnly);
    assert_eq!(spec("Kit").runtime, Runtime::Codex);
    assert_eq!(spec("Kit").model, "gpt-6.1-sol");
    assert_eq!(spec("Kit").permission_mode, PermissionMode::EditFiles);
    assert!(agents.iter().all(|a| a.spec.name != "Quill"));
}

/// crewd answers a slow check with what it found on `PATH`, then sends
/// the rest; `agent add` waits for it.
#[test]
fn agent_add_waits_for_a_slow_codex_model_list() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Well past crewd's one second quick answer.
    start_crewd_with(
        &home,
        replay_script(tmp.path()),
        Some(fake_codex_after(tmp.path(), 2.5)),
    );
    let out = stdout_of(crew(&home, tmp.path()).args([
        "agent",
        "add",
        "Nova",
        "--runtime",
        "codex",
        "--role",
        "Builds",
    ]));
    assert!(out.contains("Nova uses gpt-6-luna (GPT-6-Luna)."), "{out}");
}

#[test]
fn commit_takes_a_repo_in_a_folder_of_repos() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("crew");
    std::fs::create_dir(&home).unwrap();
    let platform = tmp.path().join("platform");
    for repo in ["api", "web"] {
        let dir = platform.join(repo);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
    }
    // Forge writes a file in each repo of its thread folder.
    let script = tmp.path().join("claude-repos.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf 'x\n' > api/x.txt
printf 'y\n' > web/y.txt
printf '{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":[]}\n'
printf '{"type":"result","subtype":"success","is_error":false,"result":"Done.","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}\n'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    start_crewd(&home, script);
    let thread = stdout_of(crew(&home, &platform).arg("init-demo"))
        .trim()
        .to_string();
    stdout_of(crew(&home, &platform).args([
        "agent", "add", "Forge", "--model", "haiku", "--role", "Builds", "--edit",
    ]));
    stdout_of(crew(&home, &platform).args(["say", &thread, "@Forge", "go"]));
    stdout_of(crew(&home, &platform).args(["watch", &thread, "--until-idle"]));

    let sha =
        stdout_of(crew(&home, &platform).args(["commit", &thread, "--repo", "api", "-m", "Add x"]));
    assert_eq!(sha.trim().len(), 8);
    let folder = std::fs::read_dir(home.join("worktrees/demo"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    assert_eq!(
        git(&folder.join("api"), &["log", "-1", "--format=%s"]),
        "Add x"
    );
    assert_eq!(
        git(&folder.join("web"), &["status", "--porcelain"]),
        "?? y.txt"
    );
}
