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
    let serve_home = home.to_path_buf();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let opts = ServeOptions {
                max_runs: 4,
                claude_bin: Some(claude_bin),
                crew_mcp_bin: None,
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
