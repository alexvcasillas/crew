//! End to end: the inspector and the approval cards, with a real crewd on
//! a temp home and a fake claude. Approvals are asked over a socket of the
//! test's own, the way crew-mcp asks them.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ApprovalId, RunId, ThreadId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::{ApprovalStatus, StoredRun};
use crew_proto::{PermissionAnswer, Request};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, ListOffset, TestAppContext, px};

use super::test_ui::{
    WAIT, click, has, label_of, notifications, open_sized, press, shows, type_text, wait_to_show,
    wait_until,
};
use super::thread_tests::{World, agent, ask, go_to, live_run, open_thread, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, short_home};

/// A claude that logs each prompt to `prompts.log` (each ends with byte
/// 0x1f). A prompt ending in `WAIT` waits for the file `go`. Then it
/// replies "ok".
fn waiting_claude(dir: &Path) -> PathBuf {
    let at = dir.display();
    let run = [
        "for last in \"$@\"; do :; done".to_string(),
        format!("printf '%s\\037' \"$last\" >> '{at}/prompts.log'"),
        format!("case \"$last\" in *WAIT) while [ ! -e '{at}/go' ]; do sleep 0.02; done;; esac"),
        r#"printf '{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":[]}\n'"#.to_string(),
        r#"printf '{"type":"result","subtype":"success","is_error":false,"result":"ok","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}\n'"#.to_string(),
    ]
    .join("\n");
    fake_claude(dir, &run)
}

fn prompts(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("prompts.log"))
        .unwrap_or_default()
        .split_terminator('\u{1f}')
        .map(String::from)
        .collect()
}

/// The newest pending approval of the thread, once there is one.
fn pending(cx: &mut TestAppContext, ws: &Entity<Workspace>, thread: ThreadId) -> ApprovalId {
    let mut found = None;
    wait_until(cx, "a pending approval", |cx| {
        found = ws.read_with(cx, |ws, _| {
            ws.state().pending_approvals(thread).last().map(|a| a.id)
        });
        found.is_some()
    });
    found.unwrap()
}

fn status_of(cx: &mut TestAppContext, ws: &Entity<Workspace>, a: ApprovalId) -> ApprovalStatus {
    ws.read_with(cx, |ws, _| ws.state().approval(a).unwrap().status)
}

/// The agent's latest run in the thread, once it has reached `status`.
fn run_of(
    cx: &mut TestAppContext,
    ws: &Entity<Workspace>,
    thread: ThreadId,
    agent: AgentId,
    status: RunStatus,
) -> StoredRun {
    let mut found = None;
    wait_until(cx, &format!("a {status:?} run"), |cx| {
        found = ws.read_with(cx, |ws, _| {
            ws.state()
                .thread_runs(thread)
                .into_iter()
                .rev()
                .find(|r| r.agent == agent && r.status == status)
                .cloned()
        });
        found.is_some()
    });
    found.unwrap()
}

/// Forge working on `body`, and its run.
fn start_forge(cx: &mut TestAppContext, ws: &Entity<Workspace>, w: &World, body: &str) -> RunId {
    w.post(body);
    live_run(cx, ws, w.thread)
}

fn hand_off(w: &World, run: RunId, to: &str, brief: &str) -> String {
    w.ask(Request::Handoff {
        run,
        agent: to.into(),
        brief: brief.into(),
    })
}

/// Asks to use Bash on its own thread, as crew-mcp would. The answer
/// comes once someone decides.
fn ask_permission(
    w: &World,
    run: RunId,
    input: serde_json::Value,
) -> mpsc::Receiver<PermissionAnswer> {
    let (tx, rx) = mpsc::channel();
    let client = w.client.clone();
    std::thread::spawn(move || {
        let answer = futures::executor::block_on(client.request(Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input,
            tool_use_id: None,
        }))
        .unwrap();
        let _ = tx.send(serde_json::from_value(answer).unwrap());
    });
    rx
}

fn answer_of(cx: &mut TestAppContext, rx: &mpsc::Receiver<PermissionAnswer>) -> PermissionAnswer {
    let mut got = None;
    wait_until(cx, "the permission answer", |_| {
        got = rx.try_recv().ok();
        got.is_some()
    });
    got.unwrap()
}

fn starts(cx: &mut TestAppContext, window: AnyWindowHandle, id: String, prefix: &str) -> bool {
    label_of(cx, window, id).is_some_and(|l| l.starts_with(prefix))
}

#[gpui_kit::test]
fn handoff_card_approves_and_target_runs(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = waiting_claude(tmp.path());
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");
    click(cx, window, "inspector-tab-thread");

    assert_eq!(
        hand_off(&w, run, "Scout", "check the edit"),
        "Asked the user to approve bringing in Scout."
    );
    let id = pending(cx, &ws, w.thread);
    let card = format!("approval-{id}");
    wait_until(cx, "the handoff card", |cx| has(cx, window, card.clone()));
    assert_eq!(
        label_of(cx, window, card.clone()).as_deref(),
        Some("Needs you · Forge wants to bring in Scout")
    );
    assert_eq!(
        label_of(cx, window, format!("approval-brief-{id}")).as_deref(),
        Some("check the edit")
    );
    assert!(shows(
        cx,
        window,
        "This thread: 0 of 3 handoffs used · Scout runs read-only"
    ));
    assert_eq!(
        label_of(cx, window, format!("participant-{forge}")).as_deref(),
        Some("Forge Waiting for you")
    );

    click(cx, window, format!("approval-approve-{id}"));
    wait_until(cx, "the card to fold", |cx| {
        starts(
            cx,
            window,
            card.clone(),
            "Handoff to Scout approved by you · ",
        )
    });
    assert!(!has(cx, window, format!("approval-approve-{id}")));
    // The budget bar follows crewd's count live.
    wait_until(cx, "one handoff used", |cx| {
        label_of(cx, window, "handoffs-used").as_deref() == Some("1 / 3")
    });

    std::fs::write(tmp.path().join("go"), "").unwrap();
    run_of(cx, &ws, w.thread, scout, RunStatus::Done);
    let sent = prompts(tmp.path());
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[1].ends_with("check the edit"), "{sent:?}");
    assert_eq!(status_of(cx, &ws, id), ApprovalStatus::Approved);
    // The inspector follows the newest run, and you can pick either.
    wait_until(cx, "Scout in the inspector", |cx| {
        label_of(cx, window, "inspector-agent").as_deref() == Some("Scout")
    });

    wait_until(cx, "Scout's run time", |cx| {
        label_of(cx, window, format!("participant-{scout}"))
            .is_some_and(|l| l.starts_with("Scout ") && l.ends_with('s'))
    });
    click(cx, window, "inspector-tab-activity");
    assert!(has(cx, window, "inspector-run-picker"));
}

#[gpui_kit::test]
fn edit_brief_changes_the_targets_prompt(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = waiting_claude(tmp.path());
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let scout = w.agents[1];
    let (window, ws) = open_thread(cx, home.path(), &w);
    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");

    hand_off(&w, run, "Scout", "check the edit");
    let id = pending(cx, &ws, w.thread);
    wait_until(cx, "the handoff card", |cx| {
        has(cx, window, format!("approval-edit-{id}"))
    });
    click(cx, window, format!("approval-edit-{id}"));
    assert!(has(cx, window, format!("approval-brief-editor-{id}")));
    assert!(has(cx, window, format!("approval-save-{id}")));
    assert!(has(cx, window, format!("approval-decline-{id}")));
    press(cx, window, "cmd-a");
    type_text(cx, window, "look at file.txt only");
    // ⌘↵ in the editor saves and approves, like Save & approve.
    press(cx, window, "cmd-enter");

    wait_until(cx, "the card to fold", |cx| {
        starts(
            cx,
            window,
            format!("approval-{id}"),
            "Handoff to Scout approved",
        )
    });
    std::fs::write(tmp.path().join("go"), "").unwrap();
    run_of(cx, &ws, w.thread, scout, RunStatus::Done);
    let sent = prompts(tmp.path());
    assert!(sent[1].ends_with("look at file.txt only"), "{sent:?}");
    assert!(!sent[1].contains("check the edit"), "{sent:?}");
}

#[gpui_kit::test]
fn permission_card_allows_the_command(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = waiting_claude(tmp.path());
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");

    let answer = ask_permission(&w, run, serde_json::json!({"command": "touch x.txt"}));
    let id = pending(cx, &ws, w.thread);
    let card = format!("approval-{id}");
    wait_until(cx, "the permission card", |cx| {
        has(cx, window, card.clone())
    });
    assert_eq!(
        label_of(cx, window, card.clone()).as_deref(),
        Some("Needs you · Forge wants to run Bash")
    );
    assert_eq!(
        label_of(cx, window, format!("approval-input-{id}")).as_deref(),
        Some("touch x.txt")
    );
    click(cx, window, format!("approval-allow-{id}"));
    let got = answer_of(cx, &answer);
    assert!(got.allow, "{got:?}");
    wait_until(cx, "the card to fold", |cx| {
        starts(cx, window, card.clone(), "Bash allowed by you · ")
    });

    // A long input starts folded; ⌘↵ from the composer allows it.
    let fields: serde_json::Map<String, serde_json::Value> = (0..10)
        .map(|i| (format!("key{i}"), serde_json::json!(i)))
        .collect();
    let answer = ask_permission(&w, run, serde_json::Value::Object(fields));
    let id = pending(cx, &ws, w.thread);
    wait_until(cx, "the second card", |cx| {
        has(cx, window, format!("approval-input-more-{id}"))
    });
    let shown = label_of(cx, window, format!("approval-input-{id}")).unwrap();
    assert_eq!(shown.lines().count(), 6, "{shown}");
    click(cx, window, format!("approval-input-more-{id}"));
    let shown = label_of(cx, window, format!("approval-input-{id}")).unwrap();
    assert_eq!(shown.lines().count(), 12, "{shown}");
    click(cx, window, "composer-input");
    press(cx, window, "cmd-enter");
    assert!(answer_of(cx, &answer).allow);
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx)),
        "",
        "⌘↵ sent nothing"
    );
    wait_until(cx, "the second card to fold", |cx| {
        starts(cx, window, format!("approval-{id}"), "Bash allowed by you")
    });
    std::thread::sleep(Duration::from_millis(100));
    cx.run_until_parked();
    assert_eq!(notifications(cx, window), 0, "approved once");

    // Deny says so to the agent.
    let answer = ask_permission(&w, run, serde_json::json!({"command": "rm -rf build"}));
    let id = pending(cx, &ws, w.thread);
    wait_until(cx, "the third card", |cx| {
        has(cx, window, format!("approval-decline-{id}"))
    });
    click(cx, window, format!("approval-decline-{id}"));
    let got = answer_of(cx, &answer);
    assert!(!got.allow);
    assert_eq!(got.message.as_deref(), Some("The user declined this."));
    wait_until(cx, "the denied line", |cx| {
        starts(
            cx,
            window,
            format!("approval-{id}"),
            "Bash denied by you · ",
        )
    });
    std::fs::write(tmp.path().join("go"), "").unwrap();
}

#[gpui_kit::test]
fn policy_switch_to_budget_runs_without_asking(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = waiting_claude(tmp.path());
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    click(cx, window, "inspector-tab-thread");
    assert!(shows(cx, window, "Ask first (selected)"));
    assert!(shows(
        cx,
        window,
        "No working copy yet. One is made when an agent first edits files."
    ));
    click(cx, window, "policy-budget");
    wait_until(cx, "the thread's policy", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().thread(w.thread).unwrap().handoff_policy == Some(HandoffPolicy::Budget)
        })
    });
    assert!(shows(cx, window, "Run with budget (selected)"));
    assert!(
        shows(cx, window, "Handoffs: Run with budget"),
        "the header follows"
    );

    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");
    assert_eq!(
        hand_off(&w, run, "Scout", "look"),
        "Scout will pick this up after you finish."
    );
    wait_to_show(cx, window, "Forge brought in Scout.");
    wait_until(cx, "one handoff used", |cx| {
        label_of(cx, window, "handoffs-used").as_deref() == Some("1 / 3")
    });
    ws.read_with(cx, |ws, _| {
        assert!(
            ws.state().pending_approvals(w.thread).is_empty(),
            "nothing asked"
        );
    });
    run_of(cx, &ws, w.thread, scout, RunStatus::Queued);

    click(cx, window, "budget-up");
    wait_until(cx, "the budget to rise", |cx| {
        label_of(cx, window, "budget-value").as_deref() == Some("4")
    });
    ws.read_with(cx, |ws, _| {
        let t = ws.state().thread(w.thread).unwrap();
        assert_eq!(t.handoff_budget, Some(4));
        assert_eq!(t.handoff_policy, Some(HandoffPolicy::Budget), "kept");
    });
    assert_eq!(
        label_of(cx, window, "handoffs-used").as_deref(),
        Some("1 / 4")
    );

    click(cx, window, "stop-thread");
    run_of(cx, &ws, w.thread, forge, RunStatus::Stopped);
    run_of(cx, &ws, w.thread, scout, RunStatus::Stopped);
}

/// A recorded session of `n` Bash calls, each with its output. The first
/// output has 20 lines.
fn long_session(path: &Path, n: usize) {
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
            (1..=20)
                .map(|l| format!("line {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            format!("done {i}")
        };
        let result = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("t{i}"), "content": text}
            ]},
            "session_id": "s"
        });
        out.push_str(&format!("{call}\n{result}\n"));
    }
    out.push_str(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}"#);
    out.push('\n');
    std::fs::write(path, out).unwrap();
}

#[gpui_kit::test]
fn activity_list_handles_5000_events(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let session = tmp.path().join("long.jsonl");
    long_session(&session, 2500);
    let claude = fake_claude(tmp.path(), &format!("cat '{}'", session.display()));
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    // Let the run finish first: opening the thread then brings its events
    // as one backlog.
    w.post("@Forge go");
    let started = Instant::now();
    loop {
        let done: Vec<StoredRun> = w.ask(Request::ListRuns {
            status: Some(RunStatus::Done),
        });
        if !done.is_empty() {
            break;
        }
        assert!(started.elapsed() < WAIT, "the run never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (window, ws) = open_thread(cx, home.path(), &w);
    wait_until(cx, "all events", |cx| {
        ws.read_with(cx, |ws, _| ws.state().events(w.thread).len() >= 5002)
    });
    wait_until(cx, "the rows", |cx| {
        ws.read_with(cx, |ws, _| ws.inspector.rows.len() == 2501)
    });

    let started = Instant::now();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "a frame took {took:?}");
    // The list follows the end and draws only what is in view.
    assert!(has(cx, window, ("activity-row", 2500usize)), "the end");
    assert!(
        !has(cx, window, ("activity-row", 100usize)),
        "rows out of view aren't drawn"
    );

    ws.read_with(cx, |ws, _| {
        ws.inspector.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        })
    });
    let started = Instant::now();
    assert!(has(cx, window, ("activity-row", 0usize)));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!has(cx, window, ("activity-row", 2400usize)));
    assert_eq!(
        label_of(cx, window, ("activity-row", 1usize)).as_deref(),
        Some("Run step 1")
    );

    // Long output shows 12 lines until you ask for all of it.
    let output = label_of(cx, window, ("activity-output", 0usize)).unwrap();
    assert_eq!(output.lines().count(), 12);
    click(cx, window, ("activity-show-all", 0usize));
    let output = label_of(cx, window, ("activity-output", 0usize)).unwrap();
    assert_eq!(output.lines().count(), 20);
}

#[gpui_kit::test]
fn inspector_shows_activity_changes_and_the_working_copy(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    git_repo(&root);
    // Writes a file in its folder, then reports an edit and a failed test.
    let claude = fake_claude(
        tmp.path(),
        &format!(
            "echo one > new.txt\ncat '{}'",
            crate::test_support::fixture("edit_and_test.jsonl").display()
        ),
    );
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &root, &[]);
    let forge: AgentId = ask(
        &w.client,
        Request::CreateAgent {
            spec: crew_core::agent::AgentSpec {
                permission_mode: PermissionMode::EditFiles,
                ..agent("Forge", "Builds")
            },
        },
    );
    let (window, ws) = open_thread(cx, home.path(), &w);
    assert!(shows(
        cx,
        window,
        "No runs yet. Mention an agent to start one."
    ));
    click(cx, window, "inspector-tab-thread");
    w.post("@Forge fix it");
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);

    wait_until(cx, "the working copy", |cx| {
        label_of(cx, window, "working-branch").as_deref() == Some("crew/fix-the-login")
            && label_of(cx, window, "working-uncommitted").as_deref() == Some("1 file · +1 −0")
    });
    assert_eq!(
        label_of(cx, window, "inspector-agent").as_deref(),
        Some("Forge")
    );
    assert!(starts(cx, window, "inspector-run-state".into(), "Done · "));

    click(cx, window, "inspector-tab-activity");
    wait_until(cx, "the activity", |cx| {
        shows(cx, window, "Edit src/lib.rs") && shows(cx, window, "Run cargo test")
    });
    assert!(shows(cx, window, "test failed"), "the failed run's output");

    click(cx, window, "inspector-tab-changes");
    assert!(shows(cx, window, "src/lib.rs"));
    assert!(has(cx, window, "changes-review"));
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

/// The thread's handoffs used and budget, as the app knows them.
fn handoffs(
    cx: &mut TestAppContext,
    ws: &Entity<Workspace>,
    thread: ThreadId,
) -> (u32, Option<u32>) {
    ws.read_with(cx, |ws, _| {
        let t = ws.state().thread(thread).unwrap();
        (t.handoffs_used, t.handoff_budget)
    })
}

#[gpui_kit::test]
fn budget_pause_allows_three_more_then_pauses_again(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = waiting_claude(tmp.path());
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(
        home.path(),
        &tmp.path().join("repo"),
        &["Forge", "Scout", "Triage"],
    );
    let () = w.ask(Request::SetThreadHandoff {
        thread: w.thread,
        policy: Some(HandoffPolicy::Budget),
        budget: Some(1),
    });
    let (window, ws) = open_thread(cx, home.path(), &w);
    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");
    assert_eq!(
        hand_off(&w, run, "Scout", "one"),
        "Scout will pick this up after you finish."
    );
    assert_eq!(
        hand_off(&w, run, "Triage", "two"),
        "This thread used its handoffs. Asked the user whether to continue."
    );
    let id = pending(cx, &ws, w.thread);
    let card = format!("approval-{id}");
    wait_until(cx, "the pause card", |cx| {
        label_of(cx, window, card.clone()).as_deref()
            == Some("Paused. This thread used its 1 handoffs.")
    });

    // A double click counts once.
    let more = format!("approval-more-{id}");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(more.clone(), cx);
        window.render_frame(cx);
        window.click(more.clone(), cx);
    })
    .unwrap();
    wait_until(cx, "the card to fold", |cx| {
        starts(
            cx,
            window,
            card.clone(),
            "Handoff to Triage approved by you",
        )
    });
    wait_until(cx, "the held-back handoff to count", |cx| {
        handoffs(cx, &ws, w.thread) == (2, Some(4))
    });
    assert_eq!(notifications(cx, window), 0);

    // With the held-back one, three more ran; the next waits again.
    for brief in ["three", "four"] {
        assert_eq!(
            hand_off(&w, run, "Scout", brief),
            "Scout will pick this up after you finish."
        );
    }
    assert_eq!(
        hand_off(&w, run, "Scout", "five"),
        "This thread used its handoffs. Asked the user whether to continue."
    );
    wait_until(cx, "four used of four", |cx| {
        label_of(cx, window, "inspector-tab-thread").is_some()
            && handoffs(cx, &ws, w.thread) == (4, Some(4))
    });
    click(cx, window, "inspector-tab-thread");
    assert_eq!(
        label_of(cx, window, "handoffs-used").as_deref(),
        Some("4 / 4")
    );
    std::fs::write(tmp.path().join("go"), "").unwrap();
    let () = w.ask(Request::StopThread { thread: w.thread });
}

#[gpui_kit::test]
fn budget_pause_end_thread_declines_and_stops(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = waiting_claude(tmp.path());
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let forge = w.agents[0];
    let () = w.ask(Request::SetThreadHandoff {
        thread: w.thread,
        policy: Some(HandoffPolicy::Budget),
        budget: Some(0),
    });
    let (window, ws) = open_thread(cx, home.path(), &w);
    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");
    hand_off(&w, run, "Scout", "look");
    let id = pending(cx, &ws, w.thread);
    wait_until(cx, "the pause card", |cx| {
        has(cx, window, format!("approval-end-{id}"))
    });
    click(cx, window, format!("approval-end-{id}"));
    let stopped = run_of(cx, &ws, w.thread, forge, RunStatus::Stopped);
    assert_eq!(stopped.id, run);
    wait_until(cx, "the declined line", |cx| {
        starts(
            cx,
            window,
            format!("approval-{id}"),
            "Handoff to Scout declined by you",
        )
    });
    assert_eq!(status_of(cx, &ws, id), ApprovalStatus::Declined);
    assert_eq!(handoffs(cx, &ws, w.thread).0, 0);
}

#[gpui_kit::test]
fn decline_and_save_and_approve_buttons(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = waiting_claude(tmp.path());
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let scout = w.agents[1];
    let (window, ws) = open_thread(cx, home.path(), &w);
    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");

    hand_off(&w, run, "Scout", "not this");
    let id = pending(cx, &ws, w.thread);
    wait_until(cx, "the first card", |cx| {
        has(cx, window, format!("approval-decline-{id}"))
    });
    click(cx, window, format!("approval-decline-{id}"));
    wait_until(cx, "the declined line", |cx| {
        starts(
            cx,
            window,
            format!("approval-{id}"),
            "Handoff to Scout declined by you",
        )
    });
    assert_eq!(status_of(cx, &ws, id), ApprovalStatus::Declined);

    hand_off(&w, run, "Scout", "check the edit");
    let id = pending(cx, &ws, w.thread);
    wait_until(cx, "the second card", |cx| {
        has(cx, window, format!("approval-edit-{id}"))
    });
    click(cx, window, format!("approval-edit-{id}"));
    press(cx, window, "cmd-a");
    type_text(cx, window, "only the README");
    click(cx, window, format!("approval-save-{id}"));
    wait_until(cx, "the approved line", |cx| {
        starts(
            cx,
            window,
            format!("approval-{id}"),
            "Handoff to Scout approved by you",
        )
    });
    std::fs::write(tmp.path().join("go"), "").unwrap();
    run_of(cx, &ws, w.thread, scout, RunStatus::Done);
    let sent = prompts(tmp.path());
    assert_eq!(sent.len(), 2, "one Scout run: {sent:?}");
    assert!(sent[1].ends_with("only the README"), "{sent:?}");
}

#[gpui_kit::test]
fn narrow_windows_hide_the_inspector_until_asked(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open_sized(cx, home.path(), 1100.);
    go_to(cx, window, &ws, w.channel, w.thread);
    assert!(!has(cx, window, "inspector"), "hidden below 1200 px");
    assert_eq!(
        label_of(cx, window, "toggle-activity").as_deref(),
        Some("Show activity")
    );
    click(cx, window, "toggle-activity");
    assert!(has(cx, window, "inspector"));
    assert_eq!(
        label_of(cx, window, "toggle-activity").as_deref(),
        Some("Hide activity")
    );
    click(cx, window, "toggle-activity");
    assert!(!has(cx, window, "inspector"));
}

#[gpui_kit::test]
fn wide_windows_show_the_inspector_until_hidden(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open_sized(cx, home.path(), 1200.);
    go_to(cx, window, &ws, w.channel, w.thread);
    assert!(has(cx, window, "inspector"), "shown from 1200 px");
    assert_eq!(
        label_of(cx, window, "toggle-activity").as_deref(),
        Some("Hide activity")
    );
    click(cx, window, "toggle-activity");
    assert!(!has(cx, window, "inspector"));
    assert_eq!(
        label_of(cx, window, "toggle-activity").as_deref(),
        Some("Show activity")
    );
}
