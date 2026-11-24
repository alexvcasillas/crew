//! End to end: the inspector's Terminal tab, with a real crewd on a temp
//! home and a fake claude that replays recorded sessions.

use std::path::Path;
use std::time::{Duration, Instant};

use crew_core::event::RunStatus;
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, TestAppContext};

use super::test_ui::{WAIT, click, has, label_of, shows, wait_until};
use super::thread_tests::{open_thread, world};
use crate::test_support::{Daemon, fake_claude, short_home};

/// A recorded session of `n` Bash calls named `step {i}`. Call 0 prints
/// 30 lines, the rest one line.
fn session(path: &Path, n: usize) {
    let mut out = String::from(
        r#"{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":["Bash"]}"#,
    );
    out.push('\n');
    for i in 0..n {
        let call = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": format!("t{i}"), "name": "Bash", "input": {"command": format!("step {i}")}}
            ]},
            "session_id": "s"
        });
        let text = if i == 0 {
            (1..=30)
                .map(|l| format!("line {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            format!("done {i}")
        };
        let result = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("t{i}"), "content": text,
                 "is_error": i == 1}
            ]},
            "session_id": "s"
        });
        out.push_str(&format!("{call}\n{result}\n"));
    }
    out.push_str(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}"#);
    out.push('\n');
    std::fs::write(path, out).unwrap();
}

fn finished_run(
    commands: usize,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Daemon,
    super::thread_tests::World,
) {
    let tmp = tempfile::tempdir().unwrap();
    let recorded = tmp.path().join("session.jsonl");
    session(&recorded, commands);
    let claude = fake_claude(tmp.path(), &format!("cat '{}'", recorded.display()));
    let home = short_home();
    let daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    // Let the run finish first: opening the thread then brings its events
    // as one backlog.
    w.post("@Forge go");
    let started = Instant::now();
    loop {
        let done: Vec<crew_core::store::StoredRun> = w.ask(Request::ListRuns {
            status: Some(RunStatus::Done),
        });
        if !done.is_empty() {
            break;
        }
        assert!(started.elapsed() < WAIT, "the run never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
    (tmp, home, daemon, w)
}

#[gpui_kit::test]
fn the_terminal_tab_lists_the_commands_with_their_output(cx: &mut TestAppContext) {
    let (_tmp, home, _daemon, w) = finished_run(2);
    let (window, ws) = open_thread(cx, home.path(), &w);
    wait_until(cx, "the events", |cx| {
        ws.read_with(cx, |ws, _| ws.state().events(w.thread).len() >= 6)
    });

    assert_eq!(
        label_of(cx, window, "inspector-tab-terminal").as_deref(),
        Some("Terminal")
    );
    click(cx, window, "inspector-tab-terminal");
    wait_until(cx, "the commands", |cx| {
        has(cx, window, ("terminal-command", 1usize))
    });
    assert_eq!(
        label_of(cx, window, ("terminal-command", 0usize)).as_deref(),
        Some("$ step 0")
    );
    assert_eq!(
        label_of(cx, window, ("terminal-command", 1usize)).as_deref(),
        Some("$ step 1")
    );
    // Output ends at its last 20 lines until you ask for all of it.
    let out = label_of(cx, window, ("terminal-output", 0usize)).unwrap();
    assert_eq!(out.lines().count(), 20);
    assert!(out.starts_with("line 11") && out.ends_with("line 30"));
    assert_eq!(
        label_of(cx, window, ("terminal-output", 1usize)).as_deref(),
        Some("done 1")
    );
    assert_eq!(
        label_of(cx, window, ("terminal-state", 0usize)).as_deref(),
        Some("Passed")
    );
    assert_eq!(
        label_of(cx, window, ("terminal-state", 1usize)).as_deref(),
        Some("Failed")
    );
    assert!(shows(cx, window, "Read only. Commands ran in /work/demo."));

    click(cx, window, ("terminal-show-all", 0usize));
    let out = label_of(cx, window, ("terminal-output", 0usize)).unwrap();
    assert_eq!(out.lines().count(), 30);
    click(cx, window, ("terminal-show-all", 0usize));
    let out = label_of(cx, window, ("terminal-output", 0usize)).unwrap();
    assert_eq!(out.lines().count(), 20);
}

#[gpui_kit::test]
fn the_terminal_tab_is_empty_before_any_command(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(
        home.path().to_path_buf(),
        fake_claude(tmp.path(), "sleep 0"),
    );
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, _ws) = open_thread(cx, home.path(), &w);
    click(cx, window, "inspector-tab-terminal");
    assert!(shows(cx, window, "No commands yet."));
}

#[gpui_kit::test]
fn two_thousand_commands_draw_only_what_is_in_view(cx: &mut TestAppContext) {
    let (_tmp, home, _daemon, w) = finished_run(2000);
    let (window, ws) = open_thread(cx, home.path(), &w);
    wait_until(cx, "all events", |cx| {
        ws.read_with(cx, |ws, _| ws.state().events(w.thread).len() >= 4002)
    });
    click(cx, window, "inspector-tab-terminal");
    wait_until(cx, "the rows", |cx| {
        ws.read_with(cx, |ws, _| ws.inspector.term.rows.len() == 2000)
    });

    let started = Instant::now();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "a frame took {took:?}");
    assert!(has(cx, window, ("terminal-row", 1999usize)), "the end");
    assert!(
        !has(cx, window, ("terminal-row", 100usize)),
        "rows out of view aren't drawn"
    );
}

fn line(id: &str, command: &str) -> String {
    let call = serde_json::json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}
        ]},
        "session_id": "s"
    });
    format!("printf '%s\\n' '{call}'\n")
}

fn done_line(id: &str) -> String {
    let result = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": "fine"}
        ]},
        "session_id": "s"
    });
    format!("printf '%s\\n' '{result}'\n")
}

const INIT: &str = r#"printf '%s\n' '{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":["Bash"]}'
"#;
const END: &str = r#"printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"ok","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}'
"#;

#[gpui_kit::test]
fn a_command_of_a_finished_run_without_a_result_did_not_finish(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let script = format!("{INIT}{}{END}", line("t1", "sleep 99"));
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), &script));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    w.post("@Forge go");
    let (window, ws) = open_thread(cx, home.path(), &w);
    wait_until(cx, "the run to end", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().live_runs(w.thread).is_empty() && ws.state().events(w.thread).len() >= 3
        })
    });
    click(cx, window, "inspector-tab-terminal");
    wait_until(cx, "the command", |cx| {
        has(cx, window, ("terminal-state", 0usize))
    });
    assert_eq!(
        label_of(cx, window, ("terminal-state", 0usize)).as_deref(),
        Some("Didn't finish")
    );
}

#[gpui_kit::test]
fn the_tab_catches_up_and_follows_a_live_run(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let go = tmp.path().join("go");
    let script = format!(
        "{INIT}{}{}while [ ! -e '{}' ]; do sleep 0.02; done\n{}{}{END}",
        line("t1", "first"),
        done_line("t1"),
        go.display(),
        line("t2", "second"),
        done_line("t2"),
    );
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), &script));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    w.post("@Forge go");
    // Events arrive while another tab is shown.
    wait_until(cx, "the first command's events", |cx| {
        ws.read_with(cx, |ws, _| ws.state().events(w.thread).len() >= 3)
    });
    assert!(ws.read_with(cx, |ws, _| ws.inspector.term.rows.is_empty()));

    click(cx, window, "inspector-tab-terminal");
    wait_until(cx, "the first command", |cx| {
        has(cx, window, ("terminal-command", 0usize))
    });
    assert!(!has(cx, window, ("terminal-command", 1usize)));
    assert_eq!(
        label_of(cx, window, ("terminal-state", 0usize)).as_deref(),
        Some("Passed")
    );

    std::fs::write(&go, "").unwrap();
    wait_until(cx, "the second command", |cx| {
        has(cx, window, ("terminal-command", 1usize))
    });
    assert_eq!(
        label_of(cx, window, ("terminal-command", 1usize)).as_deref(),
        Some("$ second")
    );
}
