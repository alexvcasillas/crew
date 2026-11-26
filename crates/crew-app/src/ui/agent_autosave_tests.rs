//! End to end: Settings › Agents saves an existing agent as you change
//! it, with a real crewd on a temp home. Text waits for a pause in typing.

use std::time::Duration;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::ids::AgentId;
use crew_core::policy::HandoffPolicy;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::settings::agents::{AUTOSAVE_PAUSE, AgentForm};
use super::test_ui::{
    click, has, label_of, open_tall, press, saved, shows, type_text, wait_saved, wait_until,
};
use super::thread_tests::{World, agent, ask, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, short_home};
use crew_proto::Request;

const AUTO_LABEL: &str = "Auto: edit files, ask only before risky commands";

/// crewd with the named agents, the app open on Settings › Agents and
/// the form of the first agent named.
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
    // The page opens on whichever agent it lists first; the tests mean
    // the first one they made.
    click(cx, window, format!("settings-agent-{}", w.agents[0]));
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
    assert!(!has(cx, window, "agent-status"), "no status line");
    assert!(saved(cx, &ws));

    // Sonnet has Auto; Forge's model has not. The model saves at once.
    click(cx, window, "agent-model-sonnet");
    wait_until(cx, "Forge on Sonnet", |cx| {
        spec(cx, &ws, forge).model == "sonnet"
    });
    pick_permission(cx, window, 1);
    wait_until(cx, "Forge in Auto", |cx| {
        spec(cx, &ws, forge).permission_mode == PermissionMode::Auto
    });
    wait_saved(cx, &ws);
}

#[gpui_kit::test]
fn typing_a_name_saves_after_a_pause(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge"]);
    let forge = w.agents[0];

    retype(cx, window, "agent-name", "Anvil");
    settle(cx);
    assert_eq!(spec(cx, &ws, forge).name, "Forge", "not while typing");
    assert!(!saved(cx, &ws), "an edit since");

    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "the new name", |cx| {
        spec(cx, &ws, forge).name == "Anvil"
    });
    wait_saved(cx, &ws);

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
    assert!(!saved(cx, &ws));

    retype(cx, window, "agent-name", "Forge");
    retype(cx, window, "agent-budget", "three");
    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "the budget error", |cx| {
        label_of(cx, window, "agent-error").is_some_and(|e| e.starts_with("Handoffs per thread"))
    });
    settle(cx);
    assert!(!saved(cx, &ws));
    let saved = spec(cx, &ws, forge);
    assert_eq!(saved.name, "Forge");
    assert_eq!(saved.handoff_budget, None);

    retype(cx, window, "agent-budget", "4");
    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "the budget", |cx| {
        spec(cx, &ws, forge).handoff_budget == Some(4)
    });
    assert!(!has(cx, window, "agent-error"));
    wait_saved(cx, &ws);
}

#[gpui_kit::test]
fn a_change_during_a_save_saves_the_latest_after_it(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge"]);
    let forge = w.agents[0];

    // Three quick clicks before crewd answers: one save in flight, the
    // latest waiting.
    let (saving, queued) = cx
        .update_window(window, |_, window, cx| {
            for id in [
                "agent-policy-ask",
                "agent-policy-channel",
                "agent-policy-budget",
            ] {
                window.render_frame(cx);
                window.click(id, cx);
            }
            let ws = ws.read(cx);
            let saving = ws
                .settings
                .as_ref()
                .unwrap()
                .agents
                .form
                .as_ref()
                .unwrap()
                .saving;
            (saving, ws.agent_saves.queued())
        })
        .unwrap();
    assert!(saving, "a save in flight");
    assert_eq!(queued, 1, "the latest waits for the save in flight");
    wait_until(cx, "the saves to end", |cx| {
        ws.read_with(cx, |ws, _| ws.agent_saves.idle())
    });
    settle(cx);
    assert_eq!(
        spec(cx, &ws, forge).handoff_policy,
        Some(HandoffPolicy::Budget),
        "the last click wins"
    );
    assert!(saved(cx, &ws));
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
    assert!(!saved(cx, &ws));
    click(cx, window, "agent-save");
    wait_until(cx, "Scout", |cx| {
        ws.read_with(cx, |ws, _| ws.state().agents().len() == 2)
    });
    wait_saved(cx, &ws);
}

/// Clicks each element in turn within one frame's work, before crewd can
/// answer anything.
fn click_now(cx: &mut TestAppContext, window: AnyWindowHandle, ids: &[String]) {
    cx.update_window(window, |_, window, cx| {
        for id in ids {
            window.render_frame(cx);
            window.click(id.clone(), cx);
        }
        window.render_frame(cx);
    })
    .unwrap();
}

fn name_in_form(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> String {
    ws.read_with(cx, |ws, cx| {
        let form = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        form.name.read(cx).value().to_string()
    })
}

#[gpui_kit::test]
fn edits_to_two_agents_during_a_save_both_land(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);
    click(cx, window, format!("settings-agent-{forge}"));

    // Forge's first save is in flight; its second and Scout's wait.
    click_now(
        cx,
        window,
        &[
            "agent-policy-ask".into(),
            "agent-policy-budget".into(),
            format!("settings-agent-{scout}"),
            "agent-policy-ask".into(),
        ],
    );
    assert_eq!(ws.read_with(cx, |ws, _| ws.agent_saves.queued()), 2);
    wait_until(cx, "the saves to end", |cx| {
        ws.read_with(cx, |ws, _| ws.agent_saves.idle())
    });
    settle(cx);
    assert_eq!(
        spec(cx, &ws, forge).handoff_policy,
        Some(HandoffPolicy::Budget)
    );
    assert_eq!(
        spec(cx, &ws, scout).handoff_policy,
        Some(HandoffPolicy::AskFirst)
    );
}

#[gpui_kit::test]
fn reopening_an_agent_keeps_what_you_typed(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);
    click(cx, window, format!("settings-agent-{forge}"));

    // Its own row: the form stays as it is.
    retype(cx, window, "agent-name", "Anvil");
    click(cx, window, format!("settings-agent-{forge}"));
    assert_eq!(name_in_form(cx, &ws), "Anvil");
    click(cx, window, "agent-policy-ask");
    wait_until(cx, "Anvil asking first", |cx| {
        let s = spec(cx, &ws, forge);
        s.name == "Anvil" && s.handoff_policy == Some(HandoffPolicy::AskFirst)
    });

    // Away and back before crewd answers: the form starts from the
    // save, not crewd's older copy.
    retype(cx, window, "agent-role", "Smith");
    click_now(
        cx,
        window,
        &[
            format!("settings-agent-{scout}"),
            format!("settings-agent-{forge}"),
        ],
    );
    let role = ws.read_with(cx, |ws, cx| {
        let form = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        form.role.read(cx).value().to_string()
    });
    assert_eq!(role, "Smith");
    click(cx, window, "agent-policy-budget");
    wait_until(cx, "the role and the budget", |cx| {
        ws.read_with(cx, |ws, _| ws.agent_saves.idle())
    });
    settle(cx);
    let s = spec(cx, &ws, forge);
    assert_eq!(s.role, "Smith", "the later save kept the role");
    assert_eq!(s.handoff_policy, Some(HandoffPolicy::Budget));
}

#[gpui_kit::test]
fn an_agent_on_a_runtime_that_isnt_ready_still_saves(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge"]);
    // No codex in this crewd.
    let nova: crew_core::ids::AgentId = ask(
        &w.client,
        Request::CreateAgent {
            spec: AgentSpec {
                runtime: Runtime::Codex,
                model: "gpt-6-luna".into(),
                ..agent("Nova", "Codex things")
            },
        },
    );
    wait_until(cx, "Nova", |cx| {
        ws.read_with(cx, |ws, _| ws.state().agent(nova).is_some())
    });
    click(cx, window, format!("settings-agent-{nova}"));
    retype(cx, window, "agent-role", "Planner");
    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "Nova's role", |cx| {
        spec(cx, &ws, nova).role == "Planner"
    });
    assert!(!has(cx, window, "agent-error"));
    wait_saved(cx, &ws);
}

#[gpui_kit::test]
fn crewds_refusal_shows_as_the_forms_error(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge"]);
    let forge = w.agents[0];
    retype(cx, window, "agent-name", "Anvil");
    // Another agent takes the name before the app hears of it.
    let _: crew_core::ids::AgentId = ask(
        &w.client,
        Request::CreateAgent {
            spec: agent("Anvil", "Elsewhere"),
        },
    );
    click_now(cx, window, &["agent-policy-ask".into()]);
    wait_until(cx, "the refusal", |cx| has(cx, window, "agent-error"));
    assert!(!saved(cx, &ws));
    assert_eq!(spec(cx, &ws, forge).name, "Forge");
}

#[gpui_kit::test]
fn a_reopened_agent_shows_a_change_made_elsewhere(cx: &mut TestAppContext) {
    let (_tmp, _home, _daemon, w, window, ws) = agents_page(cx, &["Forge", "Scout"]);
    let (forge, scout) = (w.agents[0], w.agents[1]);
    retype(cx, window, "agent-role", "Smith");
    cx.executor().advance_clock(AUTOSAVE_PAUSE);
    wait_until(cx, "the role", |cx| spec(cx, &ws, forge).role == "Smith");
    wait_saved(cx, &ws);
    click(cx, window, format!("settings-agent-{scout}"));

    // Another client changes it.
    let changed = AgentSpec {
        role: "Welder".into(),
        ..spec(cx, &ws, forge)
    };
    let _: serde_json::Value = ask(
        &w.client,
        Request::UpdateAgent {
            agent: forge,
            spec: changed,
        },
    );
    wait_until(cx, "the change", |cx| spec(cx, &ws, forge).role == "Welder");

    click(cx, window, format!("settings-agent-{forge}"));
    assert_eq!(form(cx, &ws, |f| f.editing), Some(forge));
    let shown = ws.read_with(cx, |ws, cx| {
        let f = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        f.role.read(cx).value().to_string()
    });
    assert_eq!(shown, "Welder");
}
