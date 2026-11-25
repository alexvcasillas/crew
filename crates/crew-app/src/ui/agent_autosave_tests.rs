//! End to end: Settings › Agents saves an existing agent as you change
//! it, with a real crewd on a temp home. Text waits for a pause in typing.

use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode};
use crew_core::ids::AgentId;
use crew_core::policy::HandoffPolicy;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::settings::agents::{AUTOSAVE_PAUSE, AgentForm};
use super::test_ui::{
    click, has, label_of, open_tall, press, shows, type_text, wait_to_show, wait_until,
};
use super::thread_tests::{World, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, short_home};

const AUTO_LABEL: &str = "Auto: edit files, ask only before risky commands";

/// crewd with the named agents, the app open on Settings › Agents and
/// the first agent's form.
fn agents_page(
    cx: &mut TestAppContext,
    names: &[&str],
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Daemon,
    World,
    AnyWindowHandle,
    Entity<Workspace>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), "exit 0"));
    let w = world(home.path(), &tmp.path().join("repo"), names);
    let (window, ws) = open_tall(cx, home.path());
    let count = names.len();
    wait_until(cx, "the runtimes and agents", |cx| {
        ws.read_with(cx, |ws, _| {
            !ws.runtimes.is_empty() && ws.state().agents().len() == count
        })
    });
    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-agents");
    (tmp, home, daemon, w, window, ws)
}

fn spec(cx: &mut TestAppContext, ws: &Entity<Workspace>, agent: AgentId) -> AgentSpec {
    ws.read_with(cx, |ws, _| ws.state().agent(agent).unwrap().spec.clone())
}

fn form<R>(cx: &mut TestAppContext, ws: &Entity<Workspace>, f: impl Fn(&AgentForm) -> R) -> R {
    ws.read_with(cx, |ws, _| {
        f(ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap())
    })
}

/// Selects all of the field's text and types `text` over it.
fn retype(cx: &mut TestAppContext, window: AnyWindowHandle, field: &str, text: &str) {
    click(cx, window, field.to_string());
    press(cx, window, "cmd-a backspace");
    if !text.is_empty() {
        type_text(cx, window, text);
    }
}

/// Lets the app run a while in real time, so crewd could answer.
fn settle(cx: &mut TestAppContext) {
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Opens the permission menu and clicks its `ix`th item.
fn pick_permission(cx: &mut TestAppContext, window: AnyWindowHandle, ix: usize) {
    click(cx, window, "agent-permission");
    assert!(shows(cx, window, AUTO_LABEL), "the menu");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(ix, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

#[gpui_kit::test]
fn changing_the_permission_saves_without_a_button(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge"]);
    let forge = w.agents[0];
    assert!(!has(cx, window, "agent-save"), "no save button");
    assert_eq!(
        label_of(cx, window, "agent-status").as_deref(),
        Some("Saved")
    );

    // Sonnet has Auto; Forge's model has not. The model saves at once.
    click(cx, window, "agent-model-sonnet");
    wait_until(cx, "Forge on Sonnet", |cx| {
        spec(cx, &ws, forge).model == "sonnet"
    });
    pick_permission(cx, window, 1);
    wait_until(cx, "Forge in Auto", |cx| {
        spec(cx, &ws, forge).permission_mode == PermissionMode::Auto
    });
    wait_to_show(cx, window, "Saved");
    assert_eq!(
        label_of(cx, window, "agent-status").as_deref(),
        Some("Saved")
    );
}

#[gpui_kit::test]
fn typing_a_name_saves_after_a_pause(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge"]);
    let forge = w.agents[0];

    retype(cx, window, "agent-name", "Anvil");
    settle(cx);
    assert_eq!(spec(cx, &ws, forge).name, "Forge", "not while typing");
    assert!(!shows(cx, window, "Saved"), "an edit since");

    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "the new name", |cx| {
        spec(cx, &ws, forge).name == "Anvil"
    });
    wait_to_show(cx, window, "Saved");

    // Leaving the field saves at once. Focus moves only in an active
    // window.
    cx.update_window(window, |_, window, _| window.activate_window())
        .unwrap();
    retype(cx, window, "agent-role", "Smith");
    click(cx, window, "agent-name");
    // The blur comes with the next frame.
    assert!(has(cx, window, "agent-role"));
    wait_until(cx, "the new role", |cx| {
        spec(cx, &ws, forge).role == "Smith"
    });
}

#[gpui_kit::test]
fn an_invalid_value_is_not_saved_and_says_why(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge", "Scout"]);
    let forge = w.agents[0];

    retype(cx, window, "agent-name", "scout");
    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "the error", |cx| {
        label_of(cx, window, "agent-error").as_deref()
            == Some("You already have an agent called scout. Pick another name.")
    });
    assert!(!has(cx, window, "agent-status"), "nothing to say");

    retype(cx, window, "agent-name", "Forge");
    retype(cx, window, "agent-budget", "three");
    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "the budget error", |cx| {
        label_of(cx, window, "agent-error").is_some_and(|e| e.starts_with("Handoffs per thread"))
    });
    settle(cx);
    assert!(!shows(cx, window, "Saved"));
    let saved = spec(cx, &ws, forge);
    assert_eq!(saved.name, "Forge");
    assert_eq!(saved.handoff_budget, None);

    retype(cx, window, "agent-budget", "4");
    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "the budget", |cx| {
        spec(cx, &ws, forge).handoff_budget == Some(4)
    });
    assert!(!has(cx, window, "agent-error"));
    wait_to_show(cx, window, "Saved");
}

#[gpui_kit::test]
fn a_change_during_a_save_saves_the_latest_after_it(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge"]);
    let forge = w.agents[0];

    // Three quick clicks before crewd answers: one save in flight, the
    // latest waiting.
    let (status, queued) = cx
        .update_window(window, |_, window, cx| {
            for id in [
                "agent-policy-ask",
                "agent-policy-channel",
                "agent-policy-budget",
            ] {
                window.render_frame(cx);
                window.click(id, cx);
            }
            window.render_frame(cx);
            let status = window
                .try_find("agent-status")
                .and_then(|e| e.label().map(str::to_string));
            (status, ws.read(cx).agent_saves.queued())
        })
        .unwrap();
    assert_eq!(status.as_deref(), Some("Saving…"));
    assert!(queued, "the latest waits for the save in flight");
    wait_until(cx, "the saves to end", |cx| {
        ws.read_with(cx, |ws, _| ws.agent_saves.idle())
    });
    settle(cx);
    assert_eq!(
        spec(cx, &ws, forge).handoff_policy,
        Some(HandoffPolicy::Budget),
        "the last click wins"
    );
    assert_eq!(
        label_of(cx, window, "agent-status").as_deref(),
        Some("Saved")
    );
}

#[gpui_kit::test]
fn switching_agents_saves_what_you_typed_first(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);

    retype(cx, window, "agent-instructions", "Keep answers short.");
    click(cx, window, format!("settings-agent-{scout}"));
    assert_eq!(form(cx, &ws, |f| f.editing), Some(scout));
    wait_until(cx, "Forge's instructions", |cx| {
        spec(cx, &ws, forge).instructions == "Keep answers short."
    });

    // Leaving settings saves too.
    retype(cx, window, "agent-role", "Reviewer");
    click(cx, window, "settings-back");
    assert!(ws.read_with(cx, |ws, _| ws.settings.is_none()));
    wait_until(cx, "Scout's role", |cx| {
        spec(cx, &ws, scout).role == "Reviewer"
    });
}

#[gpui_kit::test]
fn a_new_agent_still_waits_for_its_button(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, _w, window, ws) = agents_page(cx, &["Forge"]);
    click(cx, window, "settings-new-agent");
    type_text(cx, window, "Scout");
    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    settle(cx);
    assert_eq!(ws.read_with(cx, |ws, _| ws.state().agents().len()), 1);
    assert!(!has(cx, window, "agent-status"));
    click(cx, window, "agent-save");
    wait_until(cx, "Scout", |cx| {
        ws.read_with(cx, |ws, _| ws.state().agents().len() == 2)
    });
    wait_to_show(cx, window, "Saved");
}
