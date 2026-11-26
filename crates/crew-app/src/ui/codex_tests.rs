//! End to end: Codex agents, with a real crewd on a temp home running the
//! scripted app-server stand-in as `codex`.

use std::path::Path;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::AgentId;
use crew_core::store::ApprovalStatus;
use crew_proto::Request;
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::common::runtime_chip;
use super::inspector_tests::run_of;
use super::settings::agents::CODEX_LOGIN;
use super::test_ui::{
    click, has, label_of, open_tall, press, shows, type_text, wait_saved, wait_to_show, wait_until,
};
use super::thread_tests::{go_to, world};
use crate::test_support::{Daemon, fake_claude, fake_codex, short_home};

/// The Codex thread the fake starts.
const TID: &str = "00000000-0000-7000-8000-000000000001";

/// crewd's model listing: two models, Luna second.
fn models(dir: &Path) {
    list_models(
        dir,
        &[("gpt-6.1-sol", "GPT-6.1-Sol"), ("gpt-6-luna", "GPT-6-Luna")],
    );
}

/// crewd's model listing: these `(id, label)` models.
fn list_models(dir: &Path, models: &[(&str, &str)]) {
    let data: Vec<Value> = models
        .iter()
        .map(|(id, label)| json!({"id": id, "model": id, "displayName": label, "hidden": false}))
        .collect();
    let steps = json!([
        {"expect": "initialize", "as": "init"},
        {"respond": "init", "result": {}},
        {"expect": "initialized"},
        {"expect": "model/list", "as": "list"},
        {"respond": "list", "result": {"data": data, "nextCursor": null}}
    ]);
    std::fs::write(dir.join("models.json"), json!({"steps": steps}).to_string()).unwrap();
}

fn item(method: &str, item: Value) -> Value {
    json!({"send": {"method": method, "params": {"item": item, "threadId": TID, "turnId": "turn-1"}}})
}

fn command(status: &str) -> Value {
    json!({"type": "commandExecution", "id": "exec-1", "command": "mkdir sub", "cwd": "/work/demo",
           "status": status, "commandActions": [],
           "aggregatedOutput": if status == "completed" { json!("") } else { Value::Null },
           "exitCode": if status == "completed" { json!(0) } else { Value::Null }})
}

/// A unified diff of `n` changed lines.
fn long_diff(n: usize) -> String {
    let mut diff = format!("--- a/notes.md\n+++ b/notes.md\n@@ -1,{n} +1,{n} @@\n");
    for i in 0..n {
        diff.push_str(&format!("-old {i}\n+new {i}\n"));
    }
    diff
}

/// The first run: a command that asks first, then a file change.
fn run_scenario(dir: &Path, diff: &str) {
    let file = |status: &str| {
        json!({"type": "fileChange", "id": "exec-2", "status": status,
               "changes": [{"path": "/work/demo/notes.md", "kind": {"type": "update", "move_path": null}, "diff": diff}]})
    };
    let steps = json!([
        {"expect": "initialize", "as": "init"},
        {"respond": "init", "result": {"userAgent": "fake", "codexHome": "/work/codex", "platformFamily": "unix", "platformOs": "macos"}},
        {"expect": "initialized"},
        {"expect": "thread/start", "as": "open", "match": {"model": "gpt-6-luna", "sandbox": "read-only"}},
        {"respond": "open", "result": {"thread": {"id": TID, "cwd": "/work/demo", "path": null}, "model": "gpt-6-luna"}},
        {"expect": "turn/start", "as": "turn", "match": {"threadId": TID}},
        {"respond": "turn", "result": {"turn": {"id": "turn-1", "status": "inProgress"}}},
        item("item/started", command("inProgress")),
        {"request": "item/commandExecution/requestApproval", "id": 0,
         "params": {"threadId": TID, "turnId": "turn-1", "itemId": "exec-1",
                    "command": "/bin/zsh -lc 'mkdir sub'", "cwd": "/work/demo",
                    "commandActions": [{"type": "unknown", "command": "mkdir sub"}]},
         "reply": {"decision": "accept"}},
        item("item/completed", command("completed")),
        item("item/started", file("inProgress")),
        item("item/completed", file("completed")),
        item("item/completed", json!({"type": "agentMessage", "id": "msg-1", "text": "Made sub, edited notes.", "phase": "final_answer"})),
        {"send": {"method": "turn/completed", "params": {"threadId": TID, "turn": {"id": "turn-1", "status": "completed", "items": [], "durationMs": 5}}}}
    ]);
    let body = json!({"log": dir.join("run-1.log"), "steps": steps});
    std::fs::write(dir.join("run-1.json"), body.to_string()).unwrap();
}

fn codex_daemon(home: &Path, dir: &Path, root: &Path, login_exit: i32) -> Daemon {
    std::fs::create_dir_all(root).unwrap();
    models(dir);
    let opts = crewd::ServeOptions {
        no_title_runs: true,
        claude_bin: Some(fake_claude(dir, "exit 0")),
        codex_bin: Some(fake_codex(dir, root, login_exit)),
        ..Default::default()
    };
    Daemon::start_with_options(home.to_path_buf(), opts)
}

fn codex_models_known(cx: &mut TestAppContext, ws: &gpui_kit::Entity<super::workspace::Workspace>) {
    wait_until(cx, "the Codex models", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.runtimes
                .iter()
                .any(|r| r.runtime == Runtime::Codex && !r.models.is_empty())
        })
    });
}

#[gpui_kit::test]
fn a_codex_agent_is_made_in_settings(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let home = short_home();
    let _daemon = codex_daemon(home.path(), tmp.path(), &root, 0);
    let _w = world(home.path(), &root, &[]);
    let (window, ws) = open_tall(cx, home.path());
    codex_models_known(cx, &ws);

    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-agents");
    wait_to_show(cx, window, "Codex Signed in");
    assert!(shows(cx, window, "Opus"), "Claude's models first");
    assert!(shows(cx, window, "Claude Code Signed in (selected)"));
    click(cx, window, "runtime-codex");
    assert!(shows(cx, window, "Codex Signed in (selected)"));
    assert!(shows(cx, window, "Claude Code Signed in"));
    assert!(
        !has(cx, window, "agent-tools"),
        "Codex has no tools to allow"
    );
    assert!(
        shows(cx, window, "GPT-6-Luna (selected)"),
        "Luna is the default"
    );
    assert!(shows(cx, window, "GPT-6.1-Sol"));
    assert!(!has(cx, window, "agent-model-opus"));
    click(cx, window, "agent-name");
    type_text(cx, window, "Nova");
    click(cx, window, "agent-save");
    wait_saved(cx, &ws);

    let nova = ws.read_with(cx, |ws, _| {
        let agents = ws.state().agents();
        assert_eq!(agents.len(), 1);
        let spec = &agents[0].spec;
        assert_eq!(spec.name, "Nova");
        assert_eq!(spec.runtime, Runtime::Codex);
        assert_eq!(spec.model, "gpt-6-luna");
        agents[0].id
    });
    wait_until(cx, "the agent's chip", |cx| {
        label_of(cx, window, format!("settings-agent-chip-{nova}")).as_deref()
            == Some("codex · luna")
    });

    // Back to Claude Code: its models again, Sonnet first.
    click(cx, window, "runtime-claude");
    assert!(shows(cx, window, "Sonnet (selected)"));
    assert!(has(cx, window, "agent-tools"));
    assert!(!has(cx, window, "agent-model-gpt-6-luna"));
}

#[gpui_kit::test]
fn a_signed_out_codex_cant_be_saved(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let home = short_home();
    let _daemon = codex_daemon(home.path(), tmp.path(), &root, 1);
    let _w = world(home.path(), &root, &[]);
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the runtimes", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.runtimes
                .iter()
                .any(|r| r.runtime == Runtime::Codex && r.signed_in == Some(false))
        })
    });

    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-agents");
    wait_to_show(cx, window, "Codex Signed out");
    assert!(shows(cx, window, CODEX_LOGIN));
    click(cx, window, "runtime-codex");
    ws.read_with(cx, |ws, _| {
        let form = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        assert_eq!(form.runtime, Runtime::Codex, "you can still pick it");
    });
    click(cx, window, "agent-name");
    type_text(cx, window, "Nova");
    click(cx, window, "agent-save");
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    ws.read_with(cx, |ws, _| {
        assert!(
            ws.state().agents().is_empty(),
            "Save is off while signed out"
        );
    });

    // Claude Code is fine, so a Claude agent saves.
    click(cx, window, "runtime-claude");
    click(cx, window, "agent-save");
    wait_saved(cx, &ws);
}

#[gpui_kit::test]
fn a_codex_run_shows_its_steps_and_asks_before_a_command(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let home = short_home();
    let diff = long_diff(15);
    let _daemon = codex_daemon(home.path(), tmp.path(), &root, 0);
    run_scenario(tmp.path(), &diff);
    let w = world(home.path(), &root, &[]);
    let nova: AgentId = w.ask(Request::CreateAgent {
        spec: AgentSpec {
            name: "Nova".into(),
            role: "Engineer".into(),
            runtime: Runtime::Codex,
            model: "gpt-6-luna".into(),
            instructions: String::new(),
            permission_mode: PermissionMode::ReadOnly,
            allowed_tools: vec![],
            handoff_policy: None,
            handoff_budget: None,
            max_run_minutes: 20,
        },
    });
    let (window, ws) = open_tall(cx, home.path());
    go_to(cx, window, &ws, w.channel, w.thread);
    w.post("@Nova make a folder and edit the notes");

    let mut asked = None;
    wait_until(cx, "the command ask", |cx| {
        asked = ws.read_with(cx, |ws, _| {
            ws.state().pending_approvals(w.thread).last().map(|a| a.id)
        });
        asked.is_some()
    });
    let id = asked.unwrap();
    let card = format!("approval-{id}");
    wait_until(cx, "the permission card", |cx| {
        has(cx, window, card.clone())
    });
    assert_eq!(
        label_of(cx, window, card.clone()).as_deref(),
        Some("Needs you · Nova wants to run Bash")
    );
    assert_eq!(
        label_of(cx, window, format!("approval-input-{id}")).as_deref(),
        // Nova is read-only: an approved command runs without the sandbox.
        Some("mkdir sub (runs outside the sandbox, in your folder)")
    );
    assert_eq!(
        label_of(cx, window, format!("approval-command-{id}")).as_deref(),
        Some("/bin/zsh -lc 'mkdir sub'")
    );
    click(cx, window, format!("approval-allow-{id}"));

    // The fake ends the turn only if the reply is an accept.
    let run = run_of(cx, &ws, w.thread, nova, RunStatus::Done);
    ws.read_with(cx, |ws, _| {
        assert_eq!(
            ws.state().approval(id).unwrap().status,
            ApprovalStatus::Approved
        );
    });
    let log = std::fs::read_to_string(tmp.path().join("run-1.log")).unwrap();
    assert!(
        log.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|m| m == json!({"id": 0, "result": {"decision": "accept"}})),
        "{log}"
    );
    wait_to_show(cx, window, "Made sub, edited notes.");

    click(cx, window, "inspector-tab-activity");
    wait_until(cx, "the run's rows", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.inspector.rows_from.map(|(r, _)| r) == Some(run.id)
        })
    });
    let rows: Vec<Option<String>> = (0..6usize)
        .map(|ix| label_of(cx, window, ("activity-row", ix)))
        .collect();
    assert!(
        // The answered ask tags its row.
        rows.iter()
            .any(|r| r.as_deref() == Some("Run mkdir sub · allowed by you")),
        "{rows:?}"
    );
    let edit = rows
        .iter()
        .position(|r| r.as_deref() == Some("Edit notes.md"))
        .unwrap_or_else(|| panic!("an edit row in {rows:?}"));
    let shown = label_of(cx, window, ("activity-patch", edit)).unwrap();
    assert_eq!(shown.lines().count(), 20, "{shown}");
    assert!(
        shown.starts_with("--- a/notes.md\n+++ b/notes.md\n@@"),
        "{shown}"
    );
    assert!(shows(cx, window, "Show all (13 more lines)"));
    click(cx, window, ("activity-patch-all", edit));
    let shown = label_of(cx, window, ("activity-patch", edit)).unwrap();
    assert_eq!(shown, diff.trim_end());
}

#[gpui_kit::test]
fn many_codex_models_go_in_a_menu(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let home = short_home();
    let _daemon = codex_daemon(home.path(), tmp.path(), &root, 0);
    list_models(
        tmp.path(),
        &[
            ("gpt-6.1-sol", "GPT-6.1-Sol"),
            ("gpt-6.1-terra", "GPT-6.1 Terra"),
            ("gpt-6-astra", "GPT-6-Astra"),
            ("gpt-6-sol", "GPT-6-Sol"),
            ("gpt-6-luna", "GPT-6-Luna"),
        ],
    );
    let _w = world(home.path(), &root, &[]);
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "five Codex models", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.runtimes
                .iter()
                .any(|r| r.runtime == Runtime::Codex && r.models.len() == 5)
        })
    });

    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-agents");
    click(cx, window, "runtime-codex");
    assert!(has(cx, window, "agent-model-menu"), "a menu, not segments");
    assert!(!has(cx, window, "agent-model-gpt-6-luna"));
    assert!(
        shows(cx, window, "GPT-6-Luna"),
        "the default shows on the button"
    );
    click(cx, window, "agent-model-menu");
    assert!(shows(cx, window, "GPT-6.1 Terra"), "the menu");
    cx.update_window(window, |_, window, cx| {
        use gpui_kit::test::TestWindowExt as _;
        window.render_frame(cx);
        window.within("popup-menu").click(1usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    ws.read_with(cx, |ws, _| {
        let form = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        assert_eq!(form.model, "gpt-6.1-terra");
    });
    click(cx, window, "agent-name");
    type_text(cx, window, "Nova");
    click(cx, window, "agent-save");
    wait_saved(cx, &ws);
    ws.read_with(cx, |ws, _| {
        let spec = &ws.state().agents()[0].spec;
        assert_eq!(spec.model, "gpt-6.1-terra");
        assert_eq!(runtime_chip(spec), "codex · terra");
    });
}
