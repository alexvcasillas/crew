//! End to end: a real crewd on a temp home with a fake claude, the window
//! connected over the socket, driven by clicks and keys.

use std::path::Path;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crew_core::store::Author;
use crew_proto::Request;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
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
pub(super) struct World {
    pub(super) client: ClientHandle,
    _events: UnboundedReceiver<ClientEvent>,
    pub(super) project: ProjectId,
    pub(super) channel: ChannelId,
    pub(super) thread: ThreadId,
    pub(super) agents: Vec<AgentId>,
}

impl World {
    pub(super) fn ask<T: DeserializeOwned>(&self, req: Request) -> T {
        ask(&self.client, req)
    }

    pub(super) fn post(&self, body: &str) {
        let _: MessageId = self.ask(Request::PostMessage {
            thread: self.thread,
            body: body.into(),
        });
    }
}

pub(super) fn ask<T: DeserializeOwned>(client: &ClientHandle, req: Request) -> T {
    let answer = futures::executor::block_on(client.request(req)).unwrap();
    serde_json::from_value(answer).unwrap()
}

pub(super) fn agent(name: &str, role: &str) -> AgentSpec {
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
pub(super) fn world(home: &Path, root: &Path, agents: &[&str]) -> World {
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
        project,
        channel,
        thread,
        agents,
    }
}

/// Opens the window and goes to the world's thread through the sidebar
/// and the channel's thread list.
pub(super) fn open_thread(
    cx: &mut TestAppContext,
    home: &Path,
    w: &World,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, ws) = open(cx, home);
    go_to(cx, window, &ws, w.channel, w.thread);
    assert!(shows(cx, window, "fix the login"), "the thread's title");
    (window, ws)
}

/// Clicks the channel in the sidebar, then the thread in its list.
pub(super) fn go_to(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    channel: ChannelId,
    thread: ThreadId,
) {
    let channel = format!("channel-{channel}");
    wait_until(cx, "the channel in the sidebar", |cx| {
        has(cx, window, channel.clone())
    });
    click(cx, window, channel);
    let row = format!("thread-{thread}");
    wait_until(cx, "the thread in the list", |cx| {
        has(cx, window, row.clone())
    });
    click(cx, window, row);
    wait_until(cx, "the thread to open", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.open_thread_id() == Some(thread) && ws.state().is_watched(thread)
        })
    });
}

fn busy(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> bool {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).busy())
}

fn composer_text(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> String {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx))
}

fn menu_open(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> bool {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).menu_open())
}

/// The first live run of `thread`, once there is one.
pub(super) fn live_run(cx: &mut TestAppContext, ws: &Entity<Workspace>, thread: ThreadId) -> RunId {
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
    assert_eq!(
        label_of(cx, window, format!("run-state-{run}")).as_deref(),
        Some("Working")
    );

    // One run per thread: the next one waits, and doesn't count as running.
    w.post("@Forge then tidy up");
    let mut queued = None;
    wait_until(cx, "the queued run", |cx| {
        queued = ws.read_with(cx, |ws, _| {
            ws.state()
                .live_runs(w.thread)
                .iter()
                .find(|r| r.status == RunStatus::Queued)
                .map(|r| r.id)
        });
        queued.is_some()
    });
    let queued = queued.unwrap();
    wait_until(cx, "the queued card", |cx| {
        label_of(cx, window, format!("run-state-{queued}")).as_deref() == Some("Queued")
    });
    assert!(
        !has(cx, window, format!("run-elapsed-{queued}")),
        "no timer yet"
    );
    assert_eq!(
        label_of(cx, window, "sidebar-running").as_deref(),
        Some("1 agent running")
    );

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
    // Agents made in the same millisecond have no set order.
    let last = label_of(cx, window, ("mention-option", 1usize)).unwrap();
    press(cx, window, "up enter");
    assert_eq!(
        composer_text(cx, &ws),
        format!("@{last} "),
        "↑ wraps to the last"
    );
    press(cx, window, "cmd-a backspace");

    type_text(cx, window, "@");

    type_text(cx, window, "sc");
    assert!(shows_within(cx, window, "mention-menu", "Scout"));
    assert!(
        !shows_within(cx, window, "mention-menu", "Forge"),
        "filtered"
    );

    press(cx, window, "backspace backspace");
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

    type_text(cx, window, " @");
    assert!(menu_open(cx, &ws));
    click(cx, window, "mention-settings");
    assert!(has(cx, window, "agent-form"), "a new agent's form");
    ws.read_with(cx, |ws, _| {
        let settings = ws.settings.as_ref().expect("settings open");
        assert_eq!(settings.page, super::settings::Page::Agents);
        assert_eq!(settings.agents.form.as_ref().unwrap().editing, None);
    });
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

#[gpui_kit::test]
fn drafts_stay_with_their_thread(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let other: ThreadId = w.ask(Request::CreateThread {
        channel: w.channel,
        title: "second".into(),
    });
    let (window, ws) = open_thread(cx, home.path(), &w);

    type_text(cx, window, "for the first");
    go_to(cx, window, &ws, w.channel, other);
    assert_eq!(composer_text(cx, &ws), "", "the other thread has its own");
    type_text(cx, window, "for the second");

    go_to(cx, window, &ws, w.channel, w.thread);
    assert_eq!(composer_text(cx, &ws), "for the first");
    press(cx, window, "enter");
    wait_until(cx, "the first thread's message", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .messages(w.thread)
                .iter()
                .any(|m| m.body == "for the first")
        })
    });

    go_to(cx, window, &ws, w.channel, other);
    assert_eq!(composer_text(cx, &ws), "for the second");
    press(cx, window, "cmd-n");
    assert_eq!(composer_text(cx, &ws), "", "a new thread starts empty");
    type_text(cx, window, "an idea");
    go_to(cx, window, &ws, w.channel, other);
    assert_eq!(composer_text(cx, &ws), "for the second");
    press(cx, window, "cmd-n");
    assert_eq!(
        composer_text(cx, &ws),
        "an idea",
        "the new-thread draft stays"
    );
    ws.read_with(cx, |ws, _| {
        assert!(
            ws.state().messages(other).is_empty(),
            "nothing went to the second"
        );
    });
}

#[gpui_kit::test]
fn a_new_thread_send_waits_and_stays_put_if_you_left(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let bugs: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "bugs".into(),
    });
    let (window, ws) = open(cx, home.path());
    let general = format!("channel-{}", w.channel);
    let bugs_row = format!("channel-{bugs}");
    wait_until(cx, "the channels", |cx| {
        has(cx, window, general.clone()) && has(cx, window, bugs_row.clone())
    });
    click(cx, window, general.clone());
    press(cx, window, "cmd-n");
    type_text(cx, window, "Draft one");

    // Send, then type ahead and leave, before crewd's answer is taken in:
    // nothing here runs the app's tasks.
    cx.update_window(window, |_, window, cx| window.press("enter", cx))
        .unwrap();
    assert!(busy(cx, &ws), "Send waits for the new thread");
    cx.update_window(window, |_, window, cx| {
        window.input("more", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert_eq!(composer_text(cx, &ws), "more", "↵ is ignored meanwhile");
    click(cx, window, bugs_row);

    let mut created = None;
    wait_until(cx, "the new thread and its message", |cx| {
        created = ws.read_with(cx, |ws, _| {
            let state = ws.state();
            state
                .threads(w.channel)
                .iter()
                .find(|t| t.title == "Draft one")
                .map(|t| t.id)
                .filter(|t| state.messages(*t).iter().any(|m| m.body == "Draft one"))
        });
        created.is_some() && !busy(cx, &ws)
    });
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.open_thread_id(), None, "you stayed where you went");
        assert_eq!(ws.nav.channel, Some(bugs));
    });

    click(cx, window, general);
    press(cx, window, "cmd-n");
    assert_eq!(composer_text(cx, &ws), "more", "the draft you left is kept");
}

#[gpui_kit::test]
fn a_thread_named_at_startup_opens_once_known(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_when_known(w.thread, window, cx));
    })
    .unwrap();
    wait_until(cx, "the thread to open", |cx| {
        ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(w.thread))
    });
    assert!(shows(cx, window, "fix the login"));
    ws.read_with(cx, |ws, _| assert_eq!(ws.nav.channel, Some(w.channel)));
}

#[gpui_kit::test]
fn a_queued_agent_shows_queued_not_working(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), "exec sleep 60");
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    let names: Vec<String> = ws.read_with(cx, |ws, _| {
        w.agents
            .iter()
            .map(|a| ws.state().agent(*a).unwrap().spec.name.clone())
            .collect()
    });
    let forge = w.agents[names.iter().position(|n| n == "Forge").unwrap()];
    let scout = w.agents[names.iter().position(|n| n == "Scout").unwrap()];

    // One run at a time per thread: Scout waits for Forge.
    w.post("@Forge look around");
    w.post("@Scout then you");
    wait_until(cx, "Scout queued", |cx| {
        label_of(cx, window, format!("agent-status-{scout}")).as_deref() == Some("Queued")
    });
    assert_eq!(
        label_of(cx, window, format!("agent-status-{forge}")).as_deref(),
        Some("Working")
    );
    assert!(shows(cx, window, "1 agent running"));

    click(cx, window, "composer-input");
    type_text(cx, window, "@");
    assert!(menu_open(cx, &ws));
    let mut words: Vec<(String, String)> = (0..2usize)
        .map(|ix| {
            (
                label_of(cx, window, ("mention-option", ix)).unwrap(),
                label_of(cx, window, ("mention-status", ix)).unwrap(),
            )
        })
        .collect();
    words.sort();
    assert_eq!(
        words,
        [
            ("Forge".to_string(), "Working".to_string()),
            ("Scout".to_string(), "Queued".to_string())
        ]
    );
    for run in ws.read_with(cx, |ws, _| {
        ws.state()
            .live_runs(w.thread)
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>()
    }) {
        let () = w.ask(Request::StopRun { run });
    }
}
