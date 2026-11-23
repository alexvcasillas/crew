//! End to end: a real crewd on a temp home with a fake claude, the window
//! connected over the socket, driven by clicks and keys.

use std::path::Path;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crew_core::store::Author;
use crew_proto::Request;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext};
use serde::de::DeserializeOwned;

use super::mention_menu::NO_AGENTS;
use super::test_ui::{
    click, has, label_of, open, press, shows, shows_within, type_text, wait_to_show, wait_until,
};
use super::workspace::Workspace;
use crate::client::{self, ClientEvent, ClientHandle};
use crate::connection::ConnectionConfig;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

/// What the test sets up through a client of its own, as the CLI would.
struct World {
    client: ClientHandle,
    _events: UnboundedReceiver<ClientEvent>,
    channel: ChannelId,
    thread: ThreadId,
    agents: Vec<AgentId>,
}

impl World {
    fn ask<T: DeserializeOwned>(&self, req: Request) -> T {
        ask(&self.client, req)
    }

    fn post(&self, body: &str) {
        let _: MessageId = self.ask(Request::PostMessage {
            thread: self.thread,
            body: body.into(),
        });
    }
}

fn ask<T: DeserializeOwned>(client: &ClientHandle, req: Request) -> T {
    let answer = futures::executor::block_on(client.request(req)).unwrap();
    serde_json::from_value(answer).unwrap()
}

fn agent(name: &str, role: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        role: role.into(),
        runtime: Runtime::ClaudeCode,
        model: "claude-haiku-4-5".into(),
        instructions: String::new(),
        permission_mode: PermissionMode::ReadOnly,
        allowed_tools: vec![],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    }
}

/// A project rooted at `root` with `#general`, the named agents and one
/// thread.
fn world(home: &Path, root: &Path, agents: &[&str]) -> World {
    std::fs::create_dir_all(root).unwrap();
    let (client, events) = client::spawn(ConnectionConfig {
        crew_home: home.to_path_buf(),
    });
    let project: ProjectId = ask(
        &client,
        Request::CreateProject {
            name: "demo".into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
        },
    );
    let channel: ChannelId = ask(
        &client,
        Request::CreateChannel {
            project,
            name: "general".into(),
        },
    );
    let agents = agents
        .iter()
        .map(|name| {
            ask(
                &client,
                Request::CreateAgent {
                    spec: agent(name, &format!("{name} things")),
                },
            )
        })
        .collect();
    let thread: ThreadId = ask(
        &client,
        Request::CreateThread {
            channel,
            title: "fix the login".into(),
        },
    );
    World {
        client,
        _events: events,
        channel,
        thread,
        agents,
    }
}

/// Opens the window and goes to the world's thread through the sidebar
/// and the channel's thread list.
fn open_thread(
    cx: &mut TestAppContext,
    home: &Path,
    w: &World,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, ws) = open(cx, home);
    let channel = format!("channel-{}", w.channel);
    wait_until(cx, "the channel in the sidebar", |cx| {
        has(cx, window, channel.clone())
    });
    click(cx, window, channel);
    let row = format!("thread-{}", w.thread);
    wait_until(cx, "the thread in the list", |cx| {
        has(cx, window, row.clone())
    });
    click(cx, window, row);
    let thread = w.thread;
    wait_until(cx, "the thread to open", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.open_thread_id() == Some(thread) && ws.state().is_watched(thread)
        })
    });
    assert!(shows(cx, window, "fix the login"), "the thread's title");
    (window, ws)
}

fn composer_text(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> String {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx))
}

fn menu_open(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> bool {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).menu_open())
}

/// The first live run of `thread`, once there is one.
fn live_run(cx: &mut TestAppContext, ws: &Entity<Workspace>, thread: ThreadId) -> RunId {
    let mut run = None;
    wait_until(cx, "a live run", |cx| {
        run = ws.read_with(cx, |ws, _| {
            ws.state().live_runs(thread).first().map(|r| r.id)
        });
        run.is_some()
    });
    run.unwrap()
}

#[gpui_kit::test]
fn posting_a_mention_runs_the_agent_and_shows_its_reply(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(
        tmp.path(),
        &format!("cat '{}'", fixture("read_file.jsonl").display()),
    );
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    assert_eq!(
        label_of(cx, window, "title-channel").as_deref(),
        Some("#general"),
        "the title bar names the channel"
    );
    assert_eq!(
        label_of(cx, window, "title-project").as_deref(),
        Some("demo"),
        "and the project"
    );
    assert!(has(cx, window, "title-path"), "and its folder");

    click(cx, window, "composer-input");
    type_text(cx, window, "@Forge read a.txt");
    press(cx, window, "enter");

    wait_to_show(cx, window, "hello");
    assert!(shows(cx, window, "@Forge read a.txt"), "your message");
    assert_eq!(composer_text(cx, &ws), "", "the composer empties on send");
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
        Some("claude · haiku"),
        "the reply carries the agent's runtime"
    );
    wait_until(cx, "the run to end", |cx| {
        ws.read_with(cx, |ws, _| ws.state().live_runs(w.thread).is_empty())
    });
}

#[gpui_kit::test]
fn run_card_shows_steps_live_and_stop_stops_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    // Edits a file, starts the tests, then hangs until stopped.
    let claude = fake_claude(
        tmp.path(),
        &format!(
            "head -n 4 '{}'\nexec sleep 60",
            fixture("edit_and_test.jsonl").display()
        ),
    );
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    w.post("@Forge fix the tests");
    let run = live_run(cx, &ws, w.thread);
    let card = format!("run-card-{run}");
    wait_until(cx, "the steps", |cx| {
        shows_within(cx, window, card.clone(), "Edited src/lib.rs")
            && shows_within(cx, window, card.clone(), "Running cargo test")
    });
    assert_eq!(
        label_of(cx, window, format!("run-counts-{run}")).as_deref(),
        Some("1 file · +1 −1")
    );
    assert!(has(cx, window, format!("run-elapsed-{run}")), "the timer");
    assert!(shows(
        cx,
        window,
        "Forge is working. A reply now is queued for its next turn."
    ));

    click(cx, window, format!("run-stop-{run}"));
    wait_to_show(cx, window, "Forge stopped.");
    wait_until(cx, "the card to go", |cx| !has(cx, window, card.clone()));
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.state().run(run).unwrap().status, RunStatus::Stopped);
    });
}

#[gpui_kit::test]
fn mention_menu_filters_and_inserts_the_agent(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    click(cx, window, "composer-input");

    type_text(cx, window, "@");
    assert!(menu_open(cx, &ws));
    assert!(shows_within(cx, window, "mention-menu", "Forge"));
    assert!(shows_within(cx, window, "mention-menu", "Scout"));

    type_text(cx, window, "sc");
    assert!(shows_within(cx, window, "mention-menu", "Scout"));
    assert!(
        !shows_within(cx, window, "mention-menu", "Forge"),
        "filtered"
    );

    press(cx, window, "backspace backspace");
    // Agents made in the same millisecond have no set order.
    let second = label_of(cx, window, ("mention-option", 1usize)).unwrap();
    press(cx, window, "down enter");
    assert_eq!(
        composer_text(cx, &ws),
        format!("@{second} "),
        "↓ chose the second"
    );
    assert!(!menu_open(cx, &ws));

    type_text(cx, window, "@fo");
    assert!(shows_within(cx, window, "mention-menu", "Forge"));
    press(cx, window, "escape");
    assert!(!menu_open(cx, &ws), "esc closes it");
    assert!(!has(cx, window, "mention-menu"));
    type_text(cx, window, "r");
    assert!(!menu_open(cx, &ws), "and it stays closed for that @");
    assert_eq!(composer_text(cx, &ws), format!("@{second} @for"));
}

#[gpui_kit::test]
fn empty_agents_mention_menu_points_to_settings(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    click(cx, window, "composer-input");

    type_text(cx, window, "@");
    assert!(menu_open(cx, &ws));
    assert!(shows_within(cx, window, "mention-menu", NO_AGENTS));
    assert!(has(cx, window, "mention-settings"), "the Settings link");
    // Enter with nothing to pick closes the menu without sending.
    press(cx, window, "enter");
    assert!(!menu_open(cx, &ws));
    assert_eq!(composer_text(cx, &ws), "@");
}

#[gpui_kit::test]
fn sidebar_shows_working_dot_while_a_run_is_live(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), "exec sleep 60");
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open(cx, home.path());
    let channel_status = format!("channel-status-{}", w.channel);
    let agent_status = format!("agent-status-{}", w.agents[0]);
    wait_until(cx, "the sidebar", |cx| {
        has(cx, window, channel_status.clone())
    });
    assert_eq!(
        label_of(cx, window, agent_status.clone()).as_deref(),
        Some("Idle")
    );
    assert_eq!(
        label_of(cx, window, channel_status.clone()).as_deref(),
        Some("")
    );

    w.post("@Forge look around");
    wait_until(cx, "the working dot", |cx| {
        label_of(cx, window, channel_status.clone()).as_deref() == Some("Working")
    });
    assert_eq!(
        label_of(cx, window, agent_status.clone()).as_deref(),
        Some("Working")
    );
    assert!(shows(cx, window, "1 agent running"));

    let run = live_run(cx, &ws, w.thread);
    let () = w.ask(Request::StopRun { run });
    // The dot goes; "Forge stopped." is one message you haven't seen.
    wait_until(cx, "the unread count", |cx| {
        label_of(cx, window, channel_status.clone()).as_deref() == Some("1 unread")
    });
    assert_eq!(label_of(cx, window, agent_status).as_deref(), Some("Idle"));
}

#[gpui_kit::test]
fn new_thread_takes_its_title_from_the_first_line(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    press(cx, window, "cmd-n");
    wait_to_show(cx, window, "New thread");
    assert_eq!(ws.read_with(cx, |ws, _| ws.open_thread_id()), None);
    type_text(cx, window, "Tidy the README");
    press(cx, window, "shift-enter");
    type_text(cx, window, "It has typos.");
    press(cx, window, "enter");

    let mut created = None;
    wait_until(cx, "the new thread to open", |cx| {
        created = ws.read_with(cx, |ws, _| ws.open_thread_id().filter(|t| *t != w.thread));
        created.is_some()
            && ws.read_with(cx, |ws, _| {
                !ws.state().messages(created.unwrap()).is_empty()
            })
    });
    let thread = created.unwrap();
    ws.read_with(cx, |ws, _| {
        let state = ws.state();
        assert_eq!(state.thread(thread).unwrap().title, "Tidy the README");
        assert_eq!(
            state.messages(thread)[0].body,
            "Tidy the README\nIt has typos."
        );
    });
    wait_to_show(cx, window, "Tidy the README\nIt has typos.");
}
