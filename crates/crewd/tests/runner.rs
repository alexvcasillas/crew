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

    let outcome = tokio::time::timeout(Duration::from_secs(9), join)
        .await
        .expect("outcome within 9 s")
        .unwrap();
    assert_eq!(outcome, RunOutcome::Stopped);
    assert!(
        started.elapsed() >= Duration::from_secs(8),
        "SIGKILL should come after 3 s + 5 s"
    );

    let pid = Pid::from_raw(pid as i32);
    assert_eq!(kill(pid, None), Err(Errno::ESRCH), "leader still exists");
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
