//! End to end: the Auto option in Settings › Agents, and the card for a
//! call Claude's safety check stopped, with both of its answers.

use std::path::{Path, PathBuf};

use crew_core::agent::{AgentSpec, PermissionMode};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ApprovalId};
use crew_core::store::{ApprovalKind, ApprovalStatus, Author};
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::inspector_tests::{git_repo, run_of};
use super::settings::agents::AUTO_NEEDS;
use super::test_ui::{
    bounds_of, click, has, label_of, open_tall, press, shows, type_text, wait_until,
};
use super::thread_tests::{World, agent, ask, open_thread, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

const AUTO_LABEL: &str = "Auto: edit files, ask only before risky commands";
const COMMAND: &str = "rm -rf /work/demo/build";
const REASON: &str = "Reason: Destructive or unwanted actions";

/// A claude whose first run tries `rm -rf /work/demo/build` and is
/// stopped by the safety check. Later runs just answer. Each run's
/// arguments go to `args-<n>`, one per line.
fn checked_claude(dir: &Path) -> PathBuf {
    let dir_text = dir.display();
    fake_claude(
        dir,
        &format!(
            r#"n=$(cat '{dir_text}/n' 2>/dev/null || echo 0)
n=$((n + 1))
echo $n > '{dir_text}/n'
printf '%s\n' "$@" > '{dir_text}/args-'$n
if [ $n = 1 ]; then
  cat '{fixture}'
else
  printf '{{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":[]}}\n'
  printf '{{"type":"result","subtype":"success","is_error":false,"result":"Done.","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}}\n'
fi"#,
            fixture = fixture("auto_blocked_result.jsonl").display()
        ),
    )
}

/// How many runs the fake claude has started.
fn runs_started(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("n"))
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// A thread with Forge, an Auto agent, in a real repository.
fn auto_world(home: &Path, root: &Path) -> (World, AgentId) {
    git_repo(root);
    let w = world(home, root, &[]);
    let forge: AgentId = ask(
        &w.client,
        Request::CreateAgent {
            spec: AgentSpec {
                permission_mode: PermissionMode::Auto,
                model: "sonnet".into(),
                ..agent("Forge", "Builds")
            },
        },
    );
    (w, forge)
}

/// The pending Blocked approval of the thread, once it came.
fn blocked_ask(cx: &mut TestAppContext, ws: &Entity<Workspace>, w: &World) -> ApprovalId {
    let mut found = None;
    wait_until(cx, "the blocked call", |cx| {
        found = ws.read_with(cx, |ws, _| {
            ws.state()
                .pending_approvals(w.thread)
                .into_iter()
                .find(|a| matches!(a.kind, ApprovalKind::Blocked { .. }))
                .map(|a| a.id)
        });
        found.is_some()
    });
    found.unwrap()
}

fn status_of(cx: &mut TestAppContext, ws: &Entity<Workspace>, id: ApprovalId) -> ApprovalStatus {
    ws.read_with(cx, |ws, _| ws.state().approval(id).unwrap().status)
}

/// Selects all of the focused field's text and types `text` over it.
fn retype(cx: &mut TestAppContext, window: AnyWindowHandle, field: &str, text: &str) {
    click(cx, window, field.to_string());
    press(cx, window, "cmd-a backspace");
    if !text.is_empty() {
        type_text(cx, window, text);
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

fn form_permission(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> PermissionMode {
    ws.read_with(cx, |ws, _| {
        let form = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        form.permission.clone()
    })
}

#[gpui_kit::test]
fn auto_is_offered_only_for_models_that_support_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), "exit 0"));
    // Forge runs on claude-haiku-4-5, which has no auto mode.
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let forge = w.agents[0];
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the runtimes", |cx| {
        ws.read_with(cx, |ws, _| {
            !ws.runtimes.is_empty() && ws.state().agent(forge).is_some()
        })
    });
    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-agents");
    assert!(shows(cx, window, "Read only (plan)"));
    assert_eq!(
        label_of(cx, window, "agent-auto-needs").as_deref(),
        Some(AUTO_NEEDS)
    );

    // Disabled: picking it changes nothing.
    pick_permission(cx, window, 1);
    assert_eq!(form_permission(cx, &ws), PermissionMode::ReadOnly);
    press(cx, window, "escape");

    // Sonnet, the current one, has it.
    retype(cx, window, "agent-other-model", "");
    wait_until(cx, "the note to go", |cx| {
        !has(cx, window, "agent-auto-needs")
    });
    pick_permission(cx, window, 1);
    assert_eq!(form_permission(cx, &ws), PermissionMode::Auto);
    assert!(shows(cx, window, AUTO_LABEL), "the picked option");
    click(cx, window, "agent-save");
    wait_until(cx, "Forge in Auto", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().agent(forge).is_some_and(|a| {
                a.spec.permission_mode == PermissionMode::Auto && a.spec.model == "sonnet"
            })
        })
    });

    // Back on an old model, the note comes back and saving says why.
    retype(cx, window, "agent-other-model", "claude-sonnet-4-5");
    wait_until(cx, "the note", |cx| has(cx, window, "agent-auto-needs"));
    click(cx, window, "agent-save");
    wait_until(cx, "the error", |cx| {
        label_of(cx, window, "agent-error").as_deref() == Some(AUTO_NEEDS)
    });
}

#[gpui_kit::test]
fn a_blocked_call_shows_a_card_and_allow_retries_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), checked_claude(tmp.path()));
    let (w, forge) = auto_world(home.path(), &tmp.path().join("repo"));
    let (window, ws) = open_thread(cx, home.path(), &w);

    w.post("@Forge clean up the build");
    let id = blocked_ask(cx, &ws, &w);
    let card = format!("approval-{id}");
    wait_until(cx, "the card", |cx| has(cx, window, card.clone()));
    assert_eq!(
        label_of(cx, window, card.clone()).as_deref(),
        Some("Needs you · Claude's safety check stopped Forge")
    );
    assert_eq!(
        label_of(cx, window, format!("approval-input-{id}")).as_deref(),
        Some(COMMAND)
    );
    assert_eq!(
        label_of(cx, window, format!("approval-reason-{id}")).as_deref(),
        Some(REASON)
    );
    assert!(has(cx, window, format!("approval-retry-{id}")));
    assert!(has(cx, window, format!("approval-decline-{id}")));

    // In the message column, like the permission card.
    let message = ws.read_with(cx, |ws, _| {
        ws.state()
            .messages(w.thread)
            .iter()
            .find(|m| m.author == Author::You)
            .map(|m| m.id)
            .unwrap()
    });
    let body = bounds_of(cx, window, format!("message-body-{message}")).unwrap();
    let bounds = bounds_of(cx, window, card.clone()).unwrap();
    assert_eq!(bounds.origin.x, body.origin.x);
    assert!(bounds.size.width.as_f32() <= 560.);

    // The run went on and ended; the card still waits, and counts.
    let first = run_of(cx, &ws, w.thread, forge, RunStatus::Done);
    assert!(has(cx, window, card.clone()));
    wait_until(cx, "the counts", |cx| {
        label_of(cx, window, "status-needs").as_deref() == Some("1 need you")
            && label_of(cx, window, "title-running").as_deref() == Some("0 running · 1 need you")
    });

    // ⌘↵ never retries it.
    press(cx, window, "cmd-enter");
    assert_eq!(status_of(cx, &ws, id), ApprovalStatus::Pending);

    click(cx, window, format!("approval-retry-{id}"));
    wait_until(cx, "the folded line", |cx| {
        status_of(cx, &ws, id) == ApprovalStatus::Approved
            && label_of(cx, window, card.clone())
                .is_some_and(|l| l.starts_with("Allowed and retried by you · "))
    });
    wait_for_run(cx, &ws, &w, forge, first.id);
    assert_eq!(runs_started(tmp.path()), 2);
    let args = std::fs::read_to_string(tmp.path().join("args-2")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let after = |flag: &str| {
        args.iter()
            .position(|a| *a == flag)
            .and_then(|i| args.get(i + 1))
            .copied()
    };
    assert_eq!(after("--permission-mode"), Some("auto"));
    assert_eq!(
        after("--settings"),
        Some(r#"{"permissions":{"allow":["Bash(rm -rf /work/demo/build)"]}}"#),
        "the rule, as it is"
    );
    assert!(after("--resume").is_some(), "the same session");
    assert!(
        args.last().unwrap().contains("The user allowed it once."),
        "{args:?}"
    );
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);
    assert!(!has(cx, window, "status-needs"));
}

/// Waits for a run of `agent` in the thread other than `not`.
fn wait_for_run(
    cx: &mut TestAppContext,
    ws: &Entity<Workspace>,
    w: &World,
    agent: AgentId,
    not: crew_core::ids::RunId,
) {
    wait_until(cx, "the retry", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .thread_runs(w.thread)
                .iter()
                .any(|r| r.agent == agent && r.id != not)
        })
    });
}

#[gpui_kit::test]
fn needs_you_dismisses_a_blocked_call_and_activity_says_why(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), checked_claude(tmp.path()));
    let (w, forge) = auto_world(home.path(), &tmp.path().join("repo"));
    let (window, ws) = open_thread(cx, home.path(), &w);

    w.post("@Forge clean up the build");
    let id = blocked_ask(cx, &ws, &w);
    let run = run_of(cx, &ws, w.thread, forge, RunStatus::Done);

    // Activity shows the stopped call as blocked, not failed.
    click(cx, window, "inspector-tab-activity");
    wait_until(cx, "the blocked row", |cx| {
        shows(cx, window, &format!("Run {COMMAND} · Blocked · {REASON}"))
    });

    press(cx, window, "cmd-2");
    wait_until(cx, "the needs you row", |cx| {
        label_of(cx, window, format!("running-ask-state-{id}")).as_deref() == Some("Blocked")
    });
    assert_eq!(
        label_of(cx, window, format!("running-ask-now-{id}")).as_deref(),
        Some(format!("Bash: {COMMAND}").as_str())
    );
    assert!(has(cx, window, format!("running-allow-{id}")));
    click(cx, window, format!("running-deny-{id}"));
    wait_until(cx, "dismissed", |cx| {
        status_of(cx, &ws, id) == ApprovalStatus::Declined
    });
    wait_until(cx, "the row to go", |cx| {
        !has(cx, window, format!("running-ask-{id}"))
    });

    // Back in the thread it folds into one line, and nothing ran again.
    press(cx, window, "cmd-1");
    super::thread_tests::go_to(cx, window, &ws, w.channel, w.thread);
    wait_until(cx, "the folded line", |cx| {
        label_of(cx, window, format!("approval-{id}"))
            .is_some_and(|l| l.starts_with("Dismissed by you · "))
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(runs_started(tmp.path()), 1);
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.state().thread_runs(w.thread).len(), 1);
        assert_eq!(ws.state().thread_runs(w.thread)[0].id, run.id);
    });
}

#[gpui_kit::test]
fn a_call_no_rule_matches_exactly_can_only_be_dismissed(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    // The stopped call has a comma in it, which no rule can allow alone.
    let lines = std::fs::read_to_string(fixture("auto_blocked_result.jsonl"))
        .unwrap()
        .replace(COMMAND, "git log --format=%h,%s");
    let stream = tmp.path().join("stream.jsonl");
    std::fs::write(&stream, lines).unwrap();
    let claude = fake_claude(tmp.path(), &format!("cat '{}'", stream.display()));
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let (w, _) = auto_world(home.path(), &tmp.path().join("repo"));
    let (window, ws) = open_thread(cx, home.path(), &w);

    w.post("@Forge show the log");
    let id = blocked_ask(cx, &ws, &w);
    wait_until(cx, "the card", |cx| {
        has(cx, window, format!("approval-{id}"))
    });
    assert!(!has(cx, window, format!("approval-retry-{id}")));
    assert!(has(cx, window, format!("approval-decline-{id}")));
    assert_eq!(
        label_of(cx, window, format!("approval-no-retry-{id}")).as_deref(),
        Some(crew_core::claude::NO_EXACT_RULE)
    );

    press(cx, window, "cmd-2");
    wait_until(cx, "the needs you row", |cx| {
        has(cx, window, format!("running-deny-{id}"))
    });
    assert!(!has(cx, window, format!("running-allow-{id}")));
    click(cx, window, format!("running-deny-{id}"));
    wait_until(cx, "dismissed", |cx| {
        status_of(cx, &ws, id) == ApprovalStatus::Declined
    });
}
