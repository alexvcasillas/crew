use std::path::PathBuf;
use std::time::{Duration, Instant};

use crew_core::claude::{ClaudeInvocation, parse_line};
use crew_core::event::Event;
use crewd::runner::{RunOutcome, spawn_run};
use nix::errno::Errno;
use nix::sys::signal::{kill, killpg};
use nix::unistd::Pid;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fake(name: &str) -> PathBuf {
    manifest_dir().join("tests/fake").join(name)
}

fn fixture(name: &str) -> PathBuf {
    manifest_dir()
        .join("../crew-core/tests/fixtures/claude")
        .join(name)
}

fn invocation(bin: PathBuf, args: Vec<String>) -> ClaudeInvocation {
    ClaudeInvocation {
        bin,
        args,
        cwd: manifest_dir(),
    }
}

#[tokio::test]
async fn streams_events_from_fixture_then_done() {
    let path = fixture("read_file.jsonl");
    let expected: Vec<Event> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .flat_map(parse_line)
        .collect();
    assert!(!expected.is_empty());

    let inv = invocation(
        fake("claude_ok.sh"),
        vec![path.to_string_lossy().into_owned()],
    );
    let (_handle, mut rx, join) = spawn_run(inv);

    let mut got = Vec::new();
    while let Some(ev) = rx.recv().await {
        got.push(ev);
    }
    assert_eq!(got, expected);
    assert_eq!(join.await.unwrap(), RunOutcome::Done);
}

#[tokio::test]
async fn missing_binary_fails_fast() {
    let inv = invocation(PathBuf::from("/nonexistent/crew-test/claude"), vec![]);
    let (_handle, _rx, join) = spawn_run(inv);

    let outcome = tokio::time::timeout(Duration::from_secs(1), join)
        .await
        .expect("outcome within 1 s")
        .unwrap();
    assert_eq!(
        outcome,
        RunOutcome::Failed("Couldn't start `claude`. Is Claude Code installed?".into())
    );
}

#[tokio::test]
async fn crash_reports_stderr_tail() {
    let inv = invocation(fake("claude_crashes.sh"), vec![]);
    let (_handle, _rx, join) = spawn_run(inv);

    match join.await.unwrap() {
        RunOutcome::Failed(msg) => {
            for line in ["first error line", "second error line", "third error line"] {
                assert!(msg.contains(line), "missing {line:?} in {msg:?}");
            }
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn stop_escalates_to_kill() {
    let inv = invocation(fake("claude_ignores_sigint.sh"), vec![]);
    let (handle, _rx, join) = spawn_run(inv);
    let pid = handle.pid().expect("spawned");

    // Let the script install its trap before stopping it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    handle.stop();
    handle.stop();

    let outcome = tokio::time::timeout(Duration::from_secs(12), join)
        .await
        .expect("outcome within 12 s")
        .unwrap();
    assert_eq!(outcome, RunOutcome::Stopped);
    assert!(
        started.elapsed() >= Duration::from_secs(8),
        "SIGKILL should come after 3 s + 5 s"
    );

    let pid = Pid::from_raw(pid as i32);
    assert_eq!(kill(pid, None), Err(Errno::ESRCH), "leader still exists");
    // Killed grandchildren are reaped by init, which can take a moment.
    let deadline = Instant::now() + Duration::from_secs(1);
    while killpg(pid, None) != Err(Errno::ESRCH) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(killpg(pid, None), Err(Errno::ESRCH), "group still exists");

    // Stopping after exit is a no-op.
    handle.stop();
}

#[tokio::test]
async fn dropped_receiver_does_not_block_run() {
    let path = fixture("read_file.jsonl");
    let inv = invocation(
        fake("claude_ok.sh"),
        vec![path.to_string_lossy().into_owned()],
    );
    let (_handle, rx, join) = spawn_run(inv);
    drop(rx);

    let outcome = tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("outcome within 5 s")
        .unwrap();
    assert_eq!(outcome, RunOutcome::Done);
}

#[tokio::test]
async fn dropping_handle_stops_run() {
    let inv = invocation(fake("claude_sleeps.sh"), vec![]);
    let (handle, _rx, join) = spawn_run(inv);
    let pid = Pid::from_raw(handle.pid().expect("spawned") as i32);
    drop(handle);

    let outcome = tokio::time::timeout(Duration::from_secs(2), join)
        .await
        .expect("outcome within 2 s")
        .unwrap();
    assert_eq!(outcome, RunOutcome::Stopped);
    assert_eq!(kill(pid, None), Err(Errno::ESRCH), "leader still exists");
}

#[tokio::test]
async fn invalid_utf8_stderr_keeps_tail() {
    let inv = invocation(fake("claude_bad_utf8.sh"), vec![]);
    let (_handle, _rx, join) = spawn_run(inv);

    match join.await.unwrap() {
        RunOutcome::Failed(msg) => {
            assert!(msg.contains("bad "), "missing bad line in {msg:?}");
            assert!(
                msg.contains("after bad line"),
                "missing next line in {msg:?}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_cwd_fails_with_folder_message() {
    let mut inv = invocation(fake("claude_ok.sh"), vec![]);
    inv.cwd = PathBuf::from("/nonexistent/crew-test/work");
    let (handle, _rx, join) = spawn_run(inv);
    assert_eq!(handle.pid(), None);
    assert_eq!(
        join.await.unwrap(),
        RunOutcome::Failed("Working folder /nonexistent/crew-test/work doesn't exist.".into())
    );
}

#[tokio::test]
async fn exit_zero_without_turn_end_fails() {
    let inv = invocation(fake("claude_ok.sh"), vec!["/dev/null".into()]);
    let (_handle, _rx, join) = spawn_run(inv);
    assert_eq!(
        join.await.unwrap(),
        RunOutcome::Failed("`claude` exited before finishing the turn.".into())
    );
}

#[tokio::test]
async fn nonzero_exit_without_stderr_reports_code() {
    let inv = invocation(fake("claude_exits_3.sh"), vec![]);
    let (_handle, _rx, join) = spawn_run(inv);
    assert_eq!(
        join.await.unwrap(),
        RunOutcome::Failed("`claude` exited with code 3.".into())
    );
}

/// Kills the leaked process whose pid the fake wrote to `pid_file`.
struct Leaked(PathBuf);

impl Drop for Leaked {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            let _ = kill(Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
        }
    }
}

async fn wait_for_pid_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::fs::read_to_string(path).map_or(true, |s| s.trim().is_empty()) {
        assert!(Instant::now() < deadline, "fake never wrote its pid");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn held_stdout_does_not_hang_the_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("leaked.pid");
    let _leaked = Leaked(pid_file.clone());
    let inv = invocation(
        fake("claude_leaks_stdout.sh"),
        vec![pid_file.to_string_lossy().into_owned()],
    );
    let (handle, mut rx, join) = spawn_run(inv);

    let outcome = tokio::time::timeout(Duration::from_secs(4), join)
        .await
        .expect("outcome within 4 s")
        .unwrap();
    assert_eq!(
        outcome,
        RunOutcome::Failed("`claude` exited before finishing the turn.".into())
    );
    // The event channel is closed too, so a watcher can finish.
    let closed = tokio::time::timeout(Duration::from_secs(1), async {
        while rx.recv().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "event channel still open");
    wait_for_pid_file(&pid_file).await;
    handle.stop();
}

#[tokio::test]
async fn stop_with_held_stdout_closes_events() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("leaked.pid");
    let _leaked = Leaked(pid_file.clone());
    let inv = invocation(
        fake("claude_leaks_stdout.sh"),
        vec![pid_file.to_string_lossy().into_owned(), "wait".into()],
    );
    let (handle, mut rx, join) = spawn_run(inv);
    wait_for_pid_file(&pid_file).await;

    handle.stop();
    // At most the 3 s + 5 s escalation plus the 2 s reader grace, while the
    // leaked child holds the pipes for 60 s.
    let outcome = tokio::time::timeout(Duration::from_secs(12), join)
        .await
        .expect("outcome within 12 s")
        .unwrap();
    assert_eq!(outcome, RunOutcome::Stopped);
    let closed = tokio::time::timeout(Duration::from_secs(1), async {
        while rx.recv().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "event channel still open after stop");
    handle.stop();
}
