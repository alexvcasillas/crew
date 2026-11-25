//! End to end: the `/` menu, picking a skill and who runs it, against a
//! real crewd with a fake claude and skill folders on disk.

use std::path::Path;

use crew_core::agent::{AgentSpec, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ThreadId};
use crew_core::store::Author;
use crew_proto::{Request, SkillSource};
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::sidebar::row_probe::row_of;
use super::test_ui::{
    click, has, label_of, open, press, shows, shows_within, type_text, wait_to_show, wait_until,
};
use super::thread_tests::{World, agent, open_thread, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

/// A skill file with a name and a description.
fn skill_file(dir: &Path, name: &str, description: &str) {
    let dir = dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nDo it.\n"),
    )
    .unwrap();
}

/// crewd with a fake claude that logs its arguments to `args.log` and
/// plays `read_file.jsonl`, Claude's own skills in `claude-home`.
fn daemon(tmp: &Path, home: &Path) -> Daemon {
    let claude = fake_claude(
        tmp,
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\ncat '{}'",
            tmp.join("args.log").display(),
            fixture("read_file.jsonl").display()
        ),
    );
    let claude_home = tmp.join("claude-home");
    skill_file(
        &claude_home.join("skills"),
        "unslop",
        "Cut filler and AI tells from any writing",
    );
    skill_file(
        &claude_home.join("skills"),
        "tdd",
        "Write a failing test first",
    );
    skill_file(
        &claude_home.join("skills"),
        "secret",
        "Hidden from the menu",
    );
    Daemon::start_with_options(
        home.to_path_buf(),
        crewd::ServeOptions {
            no_title_runs: true,
            claude_bin: Some(claude),
            claude_home: Some(claude_home),
            ..Default::default()
        },
    )
}

/// The project's own skills, `/bench` and `/unused-deps`, and a world on
/// it with the named agents. `/secret` is hidden.
fn skills_world(tmp: &Path, home: &Path, agents: &[&str]) -> World {
    let root = tmp.join("repo");
    skill_file(
        &root.join(".claude/skills"),
        "bench",
        "Run the parse benchmarks",
    );
    skill_file(
        &root.join(".claude/skills"),
        "unused-deps",
        "Find dependencies nothing imports",
    );
    let w = world(home, &root, agents);
    let _: serde_json::Value = w.ask(Request::SetSkillShown {
        runtime: Runtime::ClaudeCode,
        source: SkillSource::Global,
        name: "secret".into(),
        shown: false,
    });
    w
}

fn args(tmp: &Path) -> Vec<String> {
    std::fs::read_to_string(tmp.join("args.log"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn composer_skill(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> Option<String> {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).skill().map(String::from))
}

/// Replaces the composer's text, as coming back to a draft does.
fn set_text(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>, text: &str) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.composer.update(cx, |c, cx| c.set_text(text, window, cx));
        });
    })
    .unwrap();
    cx.run_until_parked();
}

fn composer_text(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> String {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx))
}

fn in_menu(cx: &mut TestAppContext, window: AnyWindowHandle, label: &str) -> bool {
    shows_within(cx, window, "slash-menu", label)
}

/// The thread's ended runs, once `n` have ended.
fn ended_runs(
    cx: &mut TestAppContext,
    ws: &Entity<Workspace>,
    thread: ThreadId,
    n: usize,
) -> Vec<crew_core::store::StoredRun> {
    let mut runs = Vec::new();
    wait_until(cx, "the runs to end", |cx| {
        runs = ws.read_with(cx, |ws, _| {
            ws.state()
                .thread_runs(thread)
                .into_iter()
                .filter(|r| r.status == RunStatus::Done)
                .cloned()
                .collect::<Vec<_>>()
        });
        runs.len() >= n
    });
    runs
}

#[gpui_kit::test]
fn the_slash_menu_lists_skills_by_source_and_filters_as_you_type(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let w = skills_world(tmp.path(), home.path(), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    // "/" in the middle of a word is just text.
    click(cx, window, "composer-input");
    type_text(cx, window, "src/");
    assert!(!has(cx, window, "slash-menu"));
    set_text(cx, window, &ws, "");

    // The / button types one and opens the menu.
    click(cx, window, "composer-slash");
    wait_to_show(cx, window, "/unslop");
    assert_eq!(composer_text(cx, &ws), "/");
    for section in ["This project", "Global"] {
        assert!(in_menu(cx, window, section), "{section}");
    }
    assert!(!in_menu(cx, window, "Recent"), "nothing used yet");
    assert!(!in_menu(cx, window, "Plugins"), "no plugins");
    for skill in ["/bench", "/unused-deps", "/unslop", "/tdd"] {
        assert!(in_menu(cx, window, skill), "{skill}");
    }
    assert!(!in_menu(cx, window, "/secret"), "hidden skills stay out");
    assert_eq!(
        label_of(cx, window, ("slash-source", 0usize)).as_deref(),
        Some("project")
    );
    assert_eq!(
        label_of(cx, window, ("slash-runtime", 0usize)).as_deref(),
        Some("Claude")
    );

    // Filtering happens here, with the list already in hand.
    type_text(cx, window, "un");
    assert!(in_menu(cx, window, "/unused-deps"));
    assert!(in_menu(cx, window, "/unslop"));
    assert!(!in_menu(cx, window, "/bench"));
    assert!(!in_menu(cx, window, "/tdd"));
    assert_eq!(
        label_of(cx, window, "slash-menu").as_deref(),
        Some("Skills matching un")
    );

    type_text(cx, window, "zip");
    assert!(shows(cx, window, "No skills match."));
    assert!(has(cx, window, "slash-manage"), "the Manage skills link");
    press(cx, window, "escape");
    assert!(!has(cx, window, "slash-menu"));
    type_text(cx, window, " please");
    assert!(!has(cx, window, "slash-menu"), "it stays closed");
    assert_eq!(composer_text(cx, &ws), "/unzip please");
}

#[gpui_kit::test]
fn a_picked_skill_runs_with_the_picked_agent_on_the_model_picked(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let w = skills_world(tmp.path(), home.path(), &["Forge", "Scout"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    click(cx, window, "composer-input");
    type_text(cx, window, "/uns");
    wait_to_show(cx, window, "/unslop");
    press(cx, window, "enter");
    assert_eq!(composer_skill(cx, &ws).as_deref(), Some("unslop"));
    assert_eq!(composer_text(cx, &ws), "", "the /uns is gone");
    assert_eq!(
        label_of(cx, window, "composer-skill").as_deref(),
        Some("/unslop")
    );
    assert!(!has(cx, window, "slash-menu"));
    // The chip sits in the text: backspace takes the space after it,
    // then the chip, and the skill with it.
    press(cx, window, "backspace");
    assert_eq!(composer_skill(cx, &ws).as_deref(), Some("unslop"));
    press(cx, window, "backspace");
    assert_eq!(composer_skill(cx, &ws), None);
    assert!(!has(cx, window, "composer-skill"));
    type_text(cx, window, "/uns");
    wait_to_show(cx, window, "/unslop");
    press(cx, window, "enter");
    assert_eq!(composer_skill(cx, &ws).as_deref(), Some("unslop"));
    // One skill per message: another / is text.
    type_text(cx, window, "/x ");
    assert!(!has(cx, window, "slash-menu"));
    set_text(cx, window, &ws, "");
    assert_eq!(composer_skill(cx, &ws), None, "a new text drops the skill");

    type_text(cx, window, "/uns");
    wait_to_show(cx, window, "/unslop");
    press(cx, window, "enter");

    // Mentioning an agent on Claude runs it with that one.
    type_text(cx, window, "@Scout ");
    assert_eq!(
        label_of(cx, window, "runs-with-agent").as_deref(),
        Some("Scout")
    );
    assert_eq!(
        label_of(cx, window, "runs-with").as_deref(),
        Some("Runs with Scout · mentioned")
    );
    assert!(!has(cx, window, "runs-with-only"), "no picker, no note");
    assert!(
        !has(cx, window, "composer-send"),
        "the board draws no Send here"
    );
    set_text(cx, window, &ws, "");
    type_text(cx, window, "/uns");
    wait_to_show(cx, window, "/unslop");
    press(cx, window, "enter");

    // No one mentioned: the first Claude agent, then the one you pick.
    type_text(cx, window, "the README intro");
    assert_eq!(
        label_of(cx, window, "runs-with-agent").as_deref(),
        Some("Forge")
    );
    assert!(has(cx, window, "runs-with-only"), "this message only");
    assert!(has(cx, window, "composer-send"), "Send in the picker state");
    assert_eq!(
        label_of(cx, window, "runs-with-model").as_deref(),
        Some("haiku"),
        "Forge's own model"
    );
    click(cx, window, "runs-with-agent");
    assert!(shows(cx, window, "Agents on Claude"));
    assert!(shows(cx, window, super::runs_with::ONLY_RUNTIME));
    click(cx, window, ("runs-with-agent-option", 0usize));
    assert!(!has(cx, window, "runs-with-agents"));
    click(cx, window, "runs-with-model");
    assert!(has(cx, window, "runs-with-models"));
    click(cx, window, ("runs-with-model-option", 1usize));
    assert_eq!(
        label_of(cx, window, "runs-with-model").as_deref(),
        Some("sonnet")
    );
    click(cx, window, "composer-send");
    assert_eq!(composer_skill(cx, &ws), None, "the composer empties");

    let runs = ended_runs(cx, &ws, w.thread, 1);
    let run = &runs[0];
    assert_eq!(run.skill.as_deref(), Some("unslop"));
    assert_eq!(run.model_override.as_deref(), Some("sonnet"));
    assert_eq!(run.agent, w.agents[0], "Forge ran it");
    let args = args(tmp.path());
    let model = args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(args[model + 1], "sonnet");
    assert!(
        args.iter().any(|a| a.starts_with("/unslop ")),
        "the prompt starts with the skill: {args:?}"
    );

    // The thread: your message with its skill, who ran it, the card.
    let mine = ws.read_with(cx, |ws, _| {
        ws.state()
            .messages(w.thread)
            .iter()
            .find(|m| m.author == Author::You)
            .map(|m| m.id)
            .unwrap()
    });
    wait_to_show(cx, window, "Ran with Forge · sonnet for this message");
    assert_eq!(
        label_of(cx, window, format!("message-body-{mine}")).as_deref(),
        Some("/unslop the README intro"),
        "the skill sits inline before the text"
    );
    assert_eq!(
        label_of(cx, window, format!("run-skill-{}", run.id)).as_deref(),
        Some("/unslop · claude · sonnet · Forge")
    );
    assert_eq!(
        label_of(cx, window, format!("run-loaded-skill-{}", run.id)).as_deref(),
        Some("Loaded skill unslop global")
    );
    // The done card lists the run's steps after the skill, as on the board.
    let card = format!("done-card-{}", run.id);
    let read = label_of(cx, window, ("run-step", 0usize)).unwrap_or_default();
    assert!(read.starts_with("Read"), "{read}");
    assert!(has(cx, window, card), "a card even with no file changed");
    let reply = ws.read_with(cx, |ws, _| {
        ws.state()
            .messages(w.thread)
            .iter()
            .find(|m| matches!(m.author, Author::Agent(_)))
            .map(|m| m.id)
            .unwrap()
    });
    assert_eq!(
        label_of(cx, window, format!("message-chip-{reply}")).as_deref(),
        Some("claude · sonnet"),
        "the reply names the model it used"
    );

    // The Activity tab starts with the skill and the model.
    click(cx, window, "inspector-tab-activity");
    let size = std::fs::metadata(tmp.path().join("claude-home/skills/unslop/SKILL.md"))
        .unwrap()
        .len();
    wait_until(cx, "the skill row", |cx| {
        label_of(cx, window, ("activity-row", 0usize)).as_deref()
            == Some(&format!("Skill unslop loaded · {size} B"))
    });
    assert_eq!(
        label_of(cx, window, ("activity-row", 1usize)).as_deref(),
        Some("Model sonnet for this message. Forge's default is haiku.")
    );

    // The skill now leads the menu.
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.recents.skills, ["global:unslop"]);
    });
    click(cx, window, "composer-input");
    type_text(cx, window, "/");
    wait_until(cx, "the menu", |cx| has(cx, window, "slash-menu"));
    assert!(in_menu(cx, window, "Recent"));
    assert_eq!(
        label_of(cx, window, ("slash-option", 0usize)).as_deref(),
        Some("/unslop")
    );
}

#[gpui_kit::test]
fn a_mention_on_the_other_runtime_gets_a_note_and_a_claude_agent(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let w = skills_world(tmp.path(), home.path(), &["Forge"]);
    let quill: AgentId = w.ask(Request::CreateAgent {
        spec: AgentSpec {
            runtime: Runtime::Codex,
            model: "gpt-test".into(),
            ..agent("Quill", "Codex things")
        },
    });
    let (window, ws) = open_thread(cx, home.path(), &w);

    click(cx, window, "composer-input");
    type_text(cx, window, "/ben");
    wait_to_show(cx, window, "/bench");
    press(cx, window, "enter");
    type_text(cx, window, "@Quill the sync changes");
    press(cx, window, "escape");
    wait_to_show(cx, window, "Quill is on Codex. /bench is a Claude skill.");
    assert_eq!(
        label_of(cx, window, "runs-with-agent").as_deref(),
        Some("Forge"),
        "the first Claude agent is suggested"
    );
    assert!(
        !has(cx, window, "composer-send"),
        "↵ sends; the board has no Send"
    );
    press(cx, window, "enter");

    let runs = ended_runs(cx, &ws, w.thread, 1);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].agent, w.agents[0], "Forge ran it");
    assert_eq!(runs[0].skill.as_deref(), Some("bench"));
    let ran = ws.read_with(cx, |ws, _| {
        ws.state()
            .thread_runs(w.thread)
            .iter()
            .any(|r| r.agent == quill)
    });
    assert!(!ran, "Quill stays mentioned and doesn't run");
    assert!(shows(cx, window, "/bench @Quill the sync changes"));
    assert!(shows(cx, window, "Ran with Forge"));
}

#[gpui_kit::test]
fn no_agent_on_the_runtime_means_no_send(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let root = tmp.path().join("repo");
    let w = world(home.path(), &root, &[]);
    let _: AgentId = w.ask(Request::CreateAgent {
        spec: AgentSpec {
            runtime: Runtime::Codex,
            model: "gpt-test".into(),
            ..agent("Quill", "Codex things")
        },
    });
    let (window, ws) = open_thread(cx, home.path(), &w);
    click(cx, window, "composer-input");
    type_text(cx, window, "/tdd");
    wait_to_show(cx, window, "/tdd");
    press(cx, window, "enter");
    type_text(cx, window, "go");
    assert_eq!(
        label_of(cx, window, "runs-with-agent").as_deref(),
        Some("No agents on Claude")
    );
    press(cx, window, "enter");
    assert_eq!(composer_text(cx, &ws), "go", "nothing was sent");
    let sent = ws.read_with(cx, |ws, _| !ws.state().messages(w.thread).is_empty());
    assert!(!sent);
}

#[gpui_kit::test]
fn a_project_without_skills_says_none_match(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let claude = fake_claude(tmp.path(), "exit 0");
    let _daemon = Daemon::start_with_options(
        home.path().to_path_buf(),
        crewd::ServeOptions {
            no_title_runs: true,
            claude_bin: Some(claude),
            claude_home: Some(tmp.path().join("empty-home")),
            ..Default::default()
        },
    );
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    click(cx, window, "composer-input");
    type_text(cx, window, "/");
    wait_to_show(cx, window, "No skills match.");
    assert!(shows(cx, window, "Keep typing to send it as text"));
    press(cx, window, "enter");
    assert!(!has(cx, window, "slash-menu"), "↵ closes it");
    assert_eq!(composer_text(cx, &ws), "/");
}

#[gpui_kit::test]
fn an_empty_channel_starts_a_thread_with_a_skill(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let w = skills_world(tmp.path(), home.path(), &["Forge"]);
    let design: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "design".into(),
    });
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the channel in the sidebar", |cx| {
        has(cx, window, row_of(design))
    });
    click(cx, window, row_of(design));
    wait_until(cx, "the empty channel's composer", |cx| {
        ws.read_with(cx, |ws, cx| {
            ws.composer.read(cx).mode() == super::composer::ComposerMode::Empty
        })
    });
    assert!(has(cx, window, "composer-slash"));
    click(cx, window, "composer-input");
    type_text(cx, window, "/ben");
    wait_to_show(cx, window, "/bench");
    press(cx, window, "enter");
    assert_eq!(composer_skill(cx, &ws).as_deref(), Some("bench"));
    assert!(has(cx, window, "runs-with"), "the same row as in a thread");
    type_text(cx, window, "against main");
    press(cx, window, "enter");

    let mut thread = None;
    wait_until(cx, "the new thread", |cx| {
        thread = ws.read_with(cx, |ws, _| ws.state().threads(design).first().map(|t| t.id));
        thread.is_some()
    });
    let runs = ended_runs(cx, &ws, thread.unwrap(), 1);
    assert_eq!(runs[0].skill.as_deref(), Some("bench"));
    let title = ws.read_with(cx, |ws, _| {
        ws.state().thread(thread.unwrap()).unwrap().title.clone()
    });
    assert_eq!(title, "against main");
}

#[gpui_kit::test]
fn a_failed_send_puts_back_the_skill_agent_and_model(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let w = skills_world(tmp.path(), home.path(), &["Forge", "Scout"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    click(cx, window, "composer-input");
    type_text(cx, window, "/uns");
    wait_to_show(cx, window, "/unslop");
    press(cx, window, "enter");
    type_text(cx, window, "the README");
    click(cx, window, "runs-with-agent");
    click(cx, window, ("runs-with-agent-option", 1usize));
    click(cx, window, "runs-with-model");
    click(cx, window, ("runs-with-model-option", 1usize));
    assert_eq!(
        label_of(cx, window, "runs-with-agent").as_deref(),
        Some("Scout")
    );

    // Scout goes before the app hears of it: crewd refuses the message.
    let _: serde_json::Value = w.ask(Request::DeleteAgent { agent: w.agents[1] });
    click(cx, window, "composer-send");
    wait_until(cx, "the draft to come back", |cx| {
        composer_text(cx, &ws) == "the README"
    });
    let picked = ws
        .read_with(cx, |ws, cx| ws.composer.read(cx).picked())
        .expect("the skill came back");
    assert_eq!(picked.skill.name, "unslop");
    assert_eq!(picked.picked_agent.as_deref(), Some("Scout"));
    assert_eq!(picked.model_choice.as_deref(), Some("sonnet"));
    assert!(has(cx, window, "composer-skill"), "its chip too");
    let sent = ws.read_with(cx, |ws, _| !ws.state().messages(w.thread).is_empty());
    assert!(!sent, "nothing was saved");
    ws.read_with(cx, |ws, _| {
        assert!(ws.recents.skills.is_empty(), "Recent waits for a send");
    });
}

#[gpui_kit::test]
fn undo_after_deleting_the_chip_brings_the_skill_back(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let w = skills_world(tmp.path(), home.path(), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    click(cx, window, "composer-input");
    type_text(cx, window, "/uns");
    wait_to_show(cx, window, "/unslop");
    press(cx, window, "enter");
    press(cx, window, "backspace backspace");
    assert_eq!(composer_skill(cx, &ws), None, "the chip is gone");
    assert!(!has(cx, window, "runs-with"));

    press(cx, window, "cmd-z");
    assert!(has(cx, window, "composer-skill"), "undo brings the chip");
    assert_eq!(composer_skill(cx, &ws).as_deref(), Some("unslop"));
    assert!(has(cx, window, "runs-with"), "and its Runs with row");
    assert!(!has(cx, window, "slash-menu"), "the menu stays closed");

    type_text(cx, window, " the README");
    assert!(!has(cx, window, "slash-menu"));
    press(cx, window, "enter");
    let runs = ended_runs(cx, &ws, w.thread, 1);
    assert_eq!(
        runs[0].skill.as_deref(),
        Some("unslop"),
        "the skill went out"
    );
    let body = ws.read_with(cx, |ws, _| {
        ws.state()
            .messages(w.thread)
            .iter()
            .find(|m| m.author == Author::You)
            .map(|m| m.body.clone())
            .unwrap()
    });
    assert_eq!(body, "the README", "without the chip's text");
}
