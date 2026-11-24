//! End to end: Running now, the status bar and Stop all, with a real crewd
//! on a temp home, two projects and a fake claude that keeps working
//! until it is stopped.

use std::path::{Path, PathBuf};

use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crew_proto::Request;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext};

use super::inspector_tests::{answer_of, ask_permission};
use super::test_ui::{click, has, label_of, open, press, wait_until};
use super::thread_tests::{World, open_thread, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, fixture, recording_proxy, short_home};

/// A claude that edits a file, starts the tests, then works until it is
/// stopped.
fn busy_claude(dir: &Path) -> PathBuf {
    fake_claude(
        dir,
        &format!(
            "head -n 4 '{}'\nexec sleep 60",
            fixture("edit_and_test.jsonl").display()
        ),
    )
}

/// A second project, `shop`, with `#general` and one thread.
struct Shop {
    project: ProjectId,
    thread: ThreadId,
}

fn shop(w: &World, root: &Path) -> Shop {
    std::fs::create_dir_all(root).unwrap();
    let project: ProjectId = w.ask(Request::CreateProject {
        name: "shop".into(),
        root: root.to_string_lossy().into_owned(),
        main_branch: "main".into(),
    });
    let channel: ChannelId = w.ask(Request::CreateChannel {
        project,
        name: "general".into(),
    });
    let thread: ThreadId = w.ask(Request::CreateThread {
        channel,
        title: "add search".into(),
    });
    Shop { project, thread }
}

fn post(w: &World, thread: ThreadId, body: &str) {
    let _: MessageId = w.ask(Request::PostMessage {
        thread,
        body: body.into(),
    });
}

/// The agent's run in the thread, once it has reached `status`.
fn run_in(
    cx: &mut TestAppContext,
    ws: &Entity<Workspace>,
    thread: ThreadId,
    agent: AgentId,
    status: RunStatus,
) -> RunId {
    let mut found = None;
    wait_until(cx, &format!("a {status:?} run"), |cx| {
        found = ws.read_with(cx, |ws, _| {
            ws.state()
                .thread_runs(thread)
                .into_iter()
                .find(|r| r.agent == agent && r.status == status)
                .map(|r| r.id)
        });
        found.is_some()
    });
    found.unwrap()
}

fn wait_label(cx: &mut TestAppContext, window: AnyWindowHandle, id: String, want: &str) {
    let mut last = None;
    wait_until(cx, &format!("{id} to read {want:?}"), |cx| {
        last = label_of(cx, window, id.clone());
        last.as_deref() == Some(want)
    });
}

fn wait_gone(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) {
    wait_until(cx, &format!("{id} to go"), |cx| {
        !has(cx, window, id.clone())
    });
}

#[gpui_kit::test]
fn running_now_lists_runs_from_two_projects(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), busy_claude(tmp.path()));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);
    let shop = shop(&w, &tmp.path().join("shop"));
    let (window, ws) = open_thread(cx, home.path(), &w);

    w.post("@Forge fix the tests");
    let forge_run = run_in(cx, &ws, w.thread, forge, RunStatus::Working);
    post(&w, shop.thread, "@Scout look around");
    let scout_run = run_in(cx, &ws, shop.thread, scout, RunStatus::Working);

    // On every view: the title bar counts and the status bar.
    wait_label(cx, window, "title-running".into(), "2 running · 0 need you");
    wait_label(
        cx,
        window,
        "status-working".into(),
        "Forge and Scout working",
    );
    assert!(!has(cx, window, "status-needs"), "nobody waits on you");
    assert!(
        !has(cx, window, "status-connection"),
        "connected says nothing"
    );
    wait_label(
        cx,
        window,
        "status-runtimes".into(),
        "Claude Code signed in",
    );
    assert!(has(cx, window, "status-stop-all"));

    press(cx, window, "cmd-2");
    assert!(has(cx, window, "running"));
    assert!(!has(cx, window, "sidebar"), "the view replaces the body");
    wait_label(cx, window, "running-group-working".into(), "WORKING · 2");
    assert!(!has(cx, window, "running-group-needs"));
    assert!(!has(cx, window, "running-group-queued"));
    assert_eq!(
        label_of(cx, window, "running-summary").as_deref(),
        Some("2 agents working across 2 projects.")
    );
    assert_eq!(
        label_of(cx, window, format!("running-run-{forge_run}")).as_deref(),
        Some("Forge · demo · #general · fix the login")
    );
    assert_eq!(
        label_of(cx, window, format!("running-run-{scout_run}")).as_deref(),
        Some("Scout · shop · #general · add search")
    );
    assert_eq!(
        label_of(cx, window, format!("running-chip-{forge_run}")).as_deref(),
        Some("claude · haiku")
    );
    // The open thread is watched, so its row shows the latest step. The
    // other one only knows its status.
    wait_label(
        cx,
        window,
        format!("running-now-{forge_run}"),
        "Running cargo test",
    );
    assert_eq!(
        label_of(cx, window, format!("running-now-{scout_run}")).as_deref(),
        Some("Working")
    );
    assert!(has(cx, window, format!("running-state-{forge_run}")));
    assert_eq!(
        label_of(cx, window, "running-nav-all").as_deref(),
        Some("Running now 2")
    );

    // A project narrows the view; clicking it again, or All projects,
    // shows everything.
    let project_row = format!("running-project-{}", shop.project);
    click(cx, window, project_row.clone());
    wait_gone(cx, window, format!("running-run-{forge_run}"));
    assert!(has(cx, window, format!("running-run-{scout_run}")));
    assert_eq!(
        label_of(cx, window, "running-group-working").as_deref(),
        Some("WORKING · 1")
    );
    click(cx, window, project_row.clone());
    assert!(has(cx, window, format!("running-run-{forge_run}")));
    click(cx, window, project_row);
    wait_gone(cx, window, format!("running-run-{forge_run}"));
    click(cx, window, "running-all-projects");
    assert!(has(cx, window, format!("running-run-{forge_run}")));

    // Esc and ⌘1 go back; the title bar button opens it again.
    press(cx, window, "escape");
    assert!(!has(cx, window, "running"));
    assert!(has(cx, window, "sidebar"));
    press(cx, window, "cmd-2");
    assert!(has(cx, window, "running"));
    press(cx, window, "cmd-1");
    assert!(!has(cx, window, "running"));
    click(cx, window, "title-running");
    assert!(has(cx, window, "running"));

    // Picking a thread opens it in place of the view.
    click(cx, window, format!("running-watch-{scout_run}"));
    assert!(!has(cx, window, "running"));
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.open_thread_id(), Some(shop.thread));
    });
}

#[gpui_kit::test]
fn stop_all_shortcut_stops_everything(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), busy_claude(tmp.path()));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);
    let shop = shop(&w, &tmp.path().join("shop"));
    // Nothing opened or clicked: the shortcut works from anywhere.
    let (window, ws) = open(cx, home.path());

    w.post("@Forge fix the tests");
    let forge_run = run_in(cx, &ws, w.thread, forge, RunStatus::Working);
    post(&w, shop.thread, "@Scout look around");
    let scout_run = run_in(cx, &ws, shop.thread, scout, RunStatus::Working);
    wait_label(cx, window, "title-running".into(), "2 running · 0 need you");

    press(cx, window, "cmd-shift-.");
    run_in(cx, &ws, w.thread, forge, RunStatus::Stopped);
    run_in(cx, &ws, shop.thread, scout, RunStatus::Stopped);
    wait_gone(cx, window, "title-running".into());
    wait_gone(cx, window, "status-working".into());

    press(cx, window, "cmd-2");
    wait_label(
        cx,
        window,
        "running-group-finished".into(),
        "FINISHED IN THE LAST HOUR · 2",
    );
    assert!(!has(cx, window, "running-group-working"));
    for run in [forge_run, scout_run] {
        let state = label_of(cx, window, format!("running-state-{run}")).unwrap_or_default();
        assert!(state.starts_with("Stopped · "), "{state}");
        assert!(has(cx, window, format!("running-open-{run}")));
        assert!(!has(cx, window, format!("running-stop-{run}")));
    }
    assert_eq!(
        label_of(cx, window, "running-summary").as_deref(),
        Some("Nothing is running.")
    );
    // Open goes to the thread.
    click(cx, window, format!("running-open-{forge_run}"));
    assert!(!has(cx, window, "running"));
    ws.read_with(cx, |ws, _| assert_eq!(ws.open_thread_id(), Some(w.thread)));
}

#[gpui_kit::test]
fn queued_run_shows_in_queued_group(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with_options(
        home.path().to_path_buf(),
        crewd::ServeOptions {
            max_runs: 1,
            claude_bin: Some(busy_claude(tmp.path())),
            ..Default::default()
        },
    );
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);
    let shop = shop(&w, &tmp.path().join("shop"));
    let (window, ws) = open(cx, home.path());

    w.post("@Forge fix the tests");
    let working = run_in(cx, &ws, w.thread, forge, RunStatus::Working);
    post(&w, shop.thread, "@Scout look around");
    let queued = run_in(cx, &ws, shop.thread, scout, RunStatus::Queued);

    // Queued runs don't count as running.
    wait_label(cx, window, "title-running".into(), "1 running · 0 need you");
    click(cx, window, "title-running");
    wait_label(cx, window, "running-group-queued".into(), "QUEUED · 1");
    assert_eq!(
        label_of(cx, window, "running-group-working").as_deref(),
        Some("WORKING · 1")
    );
    assert_eq!(
        label_of(cx, window, format!("running-state-{queued}")).as_deref(),
        Some("Queued")
    );
    assert_eq!(
        label_of(cx, window, format!("running-now-{queued}")).as_deref(),
        Some("Starts when a run slot is free")
    );
    assert_eq!(
        label_of(cx, window, "running-nav-all").as_deref(),
        Some("Running now 1")
    );
    assert!(has(cx, window, format!("running-run-{working}")));

    // Stop on the row drops it from the queue.
    click(cx, window, format!("running-stop-{queued}"));
    run_in(cx, &ws, shop.thread, scout, RunStatus::Stopped);
    wait_gone(cx, window, "running-group-queued".into());
    wait_label(
        cx,
        window,
        "running-group-finished".into(),
        "FINISHED IN THE LAST HOUR · 1",
    );

    // The status bar's Stop all ends the rest.
    click(cx, window, "status-stop-all");
    run_in(cx, &ws, w.thread, forge, RunStatus::Stopped);
    wait_gone(cx, window, "running-group-working".into());
    wait_label(
        cx,
        window,
        "running-group-finished".into(),
        "FINISHED IN THE LAST HOUR · 2",
    );
}

#[gpui_kit::test]
fn needs_you_row_approves_inline(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), busy_claude(tmp.path()));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let forge = w.agents[0];
    let (window, ws) = open(cx, home.path());

    w.post("@Forge fix the tests");
    let run = run_in(cx, &ws, w.thread, forge, RunStatus::Working);
    let answer = ask_permission(&w, run, serde_json::json!({ "command": "npm test" }));
    let mut asked = None;
    wait_until(cx, "the permission ask", |cx| {
        asked = ws.read_with(cx, |ws, _| {
            ws.state().needs_you().first().map(|n| n.approval.id)
        });
        asked.is_some()
    });
    let id = asked.unwrap();

    // A run waiting on you still runs, and its ask counts on its own.
    wait_label(cx, window, "title-running".into(), "1 running · 1 need you");
    wait_label(cx, window, "status-needs".into(), "1 need you");
    assert!(has(cx, window, "status-working"));
    wait_label(cx, window, "sidebar-running".into(), "1 agent running");
    press(cx, window, "cmd-2");
    wait_label(cx, window, "running-group-needs".into(), "NEEDS YOU · 1");
    assert!(!has(cx, window, "running-group-working"), "it waits on you");
    assert_eq!(
        label_of(cx, window, "running-nav-needs").as_deref(),
        Some("Needs you 1")
    );
    assert_eq!(
        label_of(cx, window, "running-nav-all").as_deref(),
        Some("Running now 0")
    );
    assert_eq!(
        label_of(cx, window, format!("running-ask-{id}")).as_deref(),
        Some("Forge · demo · #general · fix the login")
    );
    assert_eq!(
        label_of(cx, window, format!("running-ask-state-{id}")).as_deref(),
        Some("Permission")
    );
    assert_eq!(
        label_of(cx, window, format!("running-ask-now-{id}")).as_deref(),
        Some("Bash: npm test")
    );
    assert!(!has(cx, window, format!("running-ask-sandbox-{id}")));
    assert_eq!(
        label_of(cx, window, "running-summary").as_deref(),
        Some("1 agent is waiting on you.")
    );
    assert!(has(cx, window, format!("running-ask-open-{id}")));

    click(cx, window, format!("running-allow-{id}"));
    assert!(answer_of(cx, &answer).allow);
    wait_gone(cx, window, format!("running-ask-{id}"));
    wait_label(cx, window, "running-group-working".into(), "WORKING · 1");
    assert!(has(cx, window, format!("running-run-{run}")));
    wait_label(cx, window, "title-running".into(), "1 running · 0 need you");
    assert!(!has(cx, window, "status-needs"));
    assert_eq!(
        label_of(cx, window, "status-working").as_deref(),
        Some("Forge working")
    );
    assert_eq!(
        label_of(cx, window, "running-nav-all").as_deref(),
        Some("Running now 1")
    );
}

/// Two Stop all presses in one update send one request; once crewd
/// answers, Stop all can be sent again.
#[gpui_kit::test]
fn stop_all_is_sent_once_while_in_flight(cx: &mut TestAppContext) {
    use gpui_kit::{Action as _, AppContext as _};
    let crewd_home = short_home();
    let _daemon = Daemon::start(crewd_home.path().to_path_buf());
    let home = short_home();
    let seen = recording_proxy(
        &home.path().join("crewd.sock"),
        crewd_home.path().join("crewd.sock"),
    );
    let stop_alls = || {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, Request::StopAll))
            .count()
    };
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the connection", |cx| {
        ws.read_with(cx, |ws, _| ws.connected())
    });

    cx.update_window(window, |_, window, cx| {
        window.dispatch_action(crate::app::StopAll.boxed_clone(), cx);
        window.dispatch_action(crate::app::StopAll.boxed_clone(), cx);
    })
    .unwrap();
    wait_until(cx, "the answer", |cx| {
        ws.read_with(cx, |ws, _| !ws.stopping_all)
    });
    assert_eq!(stop_alls(), 1);

    press(cx, window, "cmd-shift-.");
    wait_until(cx, "the second Stop all", |_| stop_alls() == 2);
}

/// A claude whose `--version` never answers still reads as installed.
#[gpui_kit::test]
fn status_bar_names_a_claude_that_wont_say_its_version(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("claude.sh");
    std::fs::write(&bin, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), bin);
    let (window, _) = open(cx, home.path());
    wait_label(
        cx,
        window,
        "status-runtimes".into(),
        "Claude Code installed",
    );
}
