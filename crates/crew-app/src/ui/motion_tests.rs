//! End to end: the live avatar ring, provider marks on runtime chips and
//! where approval cards sit in a thread.

use std::path::{Path, PathBuf};

use crew_core::agent::{AgentSpec, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::AgentId;
use crew_core::policy::HandoffPolicy;
use crew_core::store::{ApprovalStatus, Author};
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, ElementId, Entity, TestAppContext};

use super::inspector_tests::{
    answer_of, ask_permission, hand_off, pending, run_of, start_forge, waiting_claude,
};
use super::test_ui::{bounds_of, click, has, label_of, press, wait_until};
use super::thread_tests::{World, agent, live_run, open_thread, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

/// A claude that starts on a step, then works until it is stopped.
fn busy_claude(dir: &Path) -> PathBuf {
    fake_claude(
        dir,
        &format!(
            "head -n 4 '{}'\nexec sleep 60",
            fixture("edit_and_test.jsonl").display()
        ),
    )
}

/// Whether `inner` is drawn inside the element `scope`.
fn has_within(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    scope: impl Into<ElementId>,
    inner: &'static str,
) -> bool {
    let scope = scope.into();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        if window.try_find(scope.clone()).is_none() {
            return false;
        }
        window.within(scope).try_find(inner).is_some()
    })
    .unwrap()
}

#[gpui_kit::test]
fn the_live_ring_hugs_the_avatar_while_working(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), busy_claude(tmp.path()));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    w.post("@Forge fix the tests");
    let run = live_run(cx, &ws, w.thread);
    let ring = format!("live-ring-{run}");
    wait_until(cx, "the ring", |cx| has(cx, window, ring.clone()));
    // Wait for the steps, so the card is taller than the avatar.
    wait_until(cx, "a step", |cx| has(cx, window, ("run-step", 0usize)));
    let ring_box = bounds_of(cx, window, ring.clone()).unwrap();
    let row = bounds_of(cx, window, format!("live-{run}")).unwrap();
    assert_eq!(ring_box.size.width, ring_box.size.height, "a square ring");
    assert_eq!(
        ring_box.size.width.as_f32(),
        40.,
        "32 px avatar, 2 gap, 2 ring"
    );
    assert!(
        row.size.height > ring_box.size.height,
        "the card is taller than the ring, which doesn't stretch to it"
    );
    assert!(
        has_within(cx, window, format!("live-chip-{run}"), "provider-claude"),
        "the live run's chip has the Claude mark"
    );

    // A queued run has no ring.
    w.post("@Forge then tidy up");
    let queued = run_of(cx, &ws, w.thread, w.agents[0], RunStatus::Queued).id;
    wait_until(cx, "the queued card", |cx| {
        label_of(cx, window, format!("run-state-{queued}")).as_deref() == Some("Queued")
    });
    assert!(has(cx, window, format!("live-{queued}")));
    assert!(!has(cx, window, format!("live-ring-{queued}")), "no ring");
    assert!(
        has(cx, window, ring.clone()),
        "the working one keeps its own"
    );

    click(cx, window, format!("run-stop-{run}"));
    click(cx, window, format!("run-stop-{queued}"));
    wait_until(cx, "the runs to end", |cx| {
        !has(cx, window, format!("live-{run}")) && !has(cx, window, format!("live-{queued}"))
    });
}

#[gpui_kit::test]
fn runtime_chips_carry_their_provider_mark(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let nova: AgentId = w.ask(Request::CreateAgent {
        spec: AgentSpec {
            runtime: Runtime::Codex,
            model: "gpt-6-luna".into(),
            ..agent("Nova", "Engineer")
        },
    });
    let forge = w.agents[0];
    let (window, ws) = open_thread(cx, home.path(), &w);
    wait_until(cx, "both agents", |cx| {
        ws.read_with(cx, |ws, _| ws.state().agents().len() == 2)
    });

    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-agents");
    let codex = format!("settings-agent-chip-{nova}");
    let claude = format!("settings-agent-chip-{forge}");
    wait_until(cx, "the agent list", |cx| {
        has(cx, window, codex.clone()) && has(cx, window, claude.clone())
    });
    assert_eq!(
        label_of(cx, window, codex.clone()).as_deref(),
        Some("codex · luna"),
        "the chip still reads as before"
    );
    assert!(has_within(cx, window, codex.clone(), "provider-openai"));
    assert!(!has_within(cx, window, codex, "provider-claude"));
    assert!(has_within(cx, window, claude.clone(), "provider-claude"));
    assert!(!has_within(cx, window, claude, "provider-openai"));
}

#[gpui_kit::test]
fn approval_cards_line_up_with_message_bodies(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), busy_claude(tmp.path()));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    w.post("@Forge fix the tests");
    let run = live_run(cx, &ws, w.thread);
    let message = ws.read_with(cx, |ws, _| {
        ws.state()
            .messages(w.thread)
            .iter()
            .find(|m| m.author == Author::You)
            .map(|m| m.id)
            .unwrap()
    });
    let rx = ask_permission(&w, run, serde_json::json!({"command": "rm -rf build"}));
    let mut asked = None;
    wait_until(cx, "the ask", |cx| {
        asked = ws.read_with(cx, |ws, _| {
            ws.state().pending_approvals(w.thread).last().map(|a| a.id)
        });
        asked.is_some()
    });
    let id = asked.unwrap();
    let card = format!("approval-{id}");
    wait_until(cx, "the card", |cx| has(cx, window, card.clone()));

    let body = bounds_of(cx, window, format!("message-body-{message}")).unwrap();
    let pending = bounds_of(cx, window, card.clone()).unwrap();
    let thread = bounds_of(cx, window, "thread-messages").unwrap();
    assert_eq!(
        pending.origin.x, body.origin.x,
        "the card starts where text does"
    );
    assert_eq!(
        (pending.origin.x - thread.origin.x).as_f32(),
        24. + 32. + 12.,
        "past the avatar"
    );
    assert!(pending.size.width.as_f32() <= 560.);

    click(cx, window, format!("approval-decline-{id}"));
    assert!(!answer_of(cx, &rx).allow);
    wait_until(cx, "the folded line", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().approval(id).unwrap().status == ApprovalStatus::Declined
        }) && has(cx, window, card.clone())
    });
    let decided = bounds_of(cx, window, card).unwrap();
    assert_eq!(decided.origin.x, body.origin.x, "the folded line too");

    click(cx, window, format!("run-stop-{run}"));
    wait_until(cx, "the run to end", |cx| {
        !has(cx, window, format!("live-{run}"))
    });
}

#[gpui_kit::test]
fn the_inspector_timer_sits_level_with_its_words(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), busy_claude(tmp.path()));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    w.post("@Forge fix the tests");
    let run = live_run(cx, &ws, w.thread);
    wait_until(cx, "the timer", |cx| has(cx, window, "inspector-elapsed"));
    let state = bounds_of(cx, window, "inspector-run-state").unwrap();
    let timer = bounds_of(cx, window, "inspector-elapsed").unwrap();
    assert_eq!(timer.size.height.as_f32(), 16.);
    assert_eq!(
        timer.center().y,
        state.center().y,
        "the roll line is centred on the words"
    );

    click(cx, window, format!("run-stop-{run}"));
    wait_until(cx, "the run to end", |cx| {
        !has(cx, window, format!("live-{run}"))
    });
}

/// Asserts the card `card` starts where your message's text does, past
/// the avatar. A card, not a folded line, stays no wider than 560 px.
fn assert_inset(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    w: &World,
    card: &str,
    pending: bool,
) {
    let message = ws.read_with(cx, |ws, _| {
        ws.state()
            .messages(w.thread)
            .iter()
            .find(|m| m.author == Author::You)
            .map(|m| m.id)
            .unwrap()
    });
    let body = bounds_of(cx, window, format!("message-body-{message}")).unwrap();
    let bounds = bounds_of(cx, window, card.to_string()).unwrap();
    let thread = bounds_of(cx, window, "thread-messages").unwrap();
    assert_eq!(
        bounds.origin.x, body.origin.x,
        "{card} starts where text does"
    );
    assert_eq!(
        (bounds.origin.x - thread.origin.x).as_f32(),
        24. + 32. + 12.
    );
    if pending {
        assert!(bounds.size.width.as_f32() <= 560.);
    }
}

#[gpui_kit::test]
fn handoff_cards_line_up_with_message_bodies(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), waiting_claude(tmp.path()));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");

    hand_off(&w, run, "Scout", "check the edit");
    let id = pending(cx, &ws, w.thread);
    let card = format!("approval-{id}");
    wait_until(cx, "the handoff card", |cx| has(cx, window, card.clone()));
    assert_inset(cx, window, &ws, &w, &card, true);

    click(cx, window, format!("approval-decline-{id}"));
    wait_until(cx, "the folded line", |cx| {
        label_of(cx, window, card.clone()).is_some_and(|l| !l.starts_with("Needs you"))
    });
    assert_inset(cx, window, &ws, &w, &card, false);
    std::fs::write(tmp.path().join("go"), "").unwrap();
    let () = w.ask(Request::StopThread { thread: w.thread });
}

#[gpui_kit::test]
fn budget_pause_cards_line_up_with_message_bodies(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), waiting_claude(tmp.path()));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let () = w.ask(Request::SetThreadHandoff {
        thread: w.thread,
        policy: Some(HandoffPolicy::Budget),
        budget: Some(1),
    });
    let (window, ws) = open_thread(cx, home.path(), &w);
    let run = start_forge(cx, &ws, &w, "@Forge do it WAIT");
    hand_off(&w, run, "Scout", "one");
    hand_off(&w, run, "Scout", "two");
    let id = pending(cx, &ws, w.thread);
    let card = format!("approval-{id}");
    wait_until(cx, "the pause card", |cx| {
        label_of(cx, window, card.clone()).is_some_and(|l| l.starts_with("Paused."))
    });
    assert_inset(cx, window, &ws, &w, &card, true);
    std::fs::write(tmp.path().join("go"), "").unwrap();
    let () = w.ask(Request::StopThread { thread: w.thread });
}
