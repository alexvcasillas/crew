//! End to end: the project overview and the agent profile, with a real
//! crewd on a temp home and a fake claude.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, Task, TestAppContext};

use super::diff_tests::repo_with_readme;
use super::inspector_tests::run_of;
use super::test_ui::{click, has, label_of, open_tall, press, shows, wait_until};
use super::thread_tests::{World, agent, ask, world};
use super::workspace::{Place, Workspace};
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

/// A claude that edits a file and runs the tests, then ends.
fn editing_claude(dir: &Path) -> PathBuf {
    fake_claude(
        dir,
        &format!("cat '{}'", fixture("edit_and_test.jsonl").display()),
    )
}

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

fn forge(w: &World, mode: PermissionMode) -> AgentId {
    ask(
        &w.client,
        Request::CreateAgent {
            spec: crew_core::agent::AgentSpec {
                permission_mode: mode,
                ..agent("Forge", "Builds")
            },
        },
    )
}

fn add_channel(w: &World, project: ProjectId, name: &str) -> ChannelId {
    w.ask(Request::CreateChannel {
        project,
        name: name.into(),
    })
}

fn wait_label(cx: &mut TestAppContext, window: AnyWindowHandle, id: String, want: &str) {
    let mut last = None;
    wait_until(cx, &format!("{id} to read {want:?}"), |cx| {
        last = label_of(cx, window, id.clone());
        last.as_deref() == Some(want)
    });
}

fn go(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>, place: Place) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_place(place, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
}

fn record_opens(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> Arc<Mutex<Vec<Vec<OsString>>>> {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let seen = opened.clone();
    ws.update(cx, |ws, _| {
        ws.opener = Arc::new(move |args| {
            seen.lock().unwrap().push(args);
            Ok(())
        });
    });
    opened
}

fn composer_text(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> String {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx))
}

#[gpui_kit::test]
fn clicking_a_project_opens_its_overview(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), editing_claude(tmp.path()));
    let w = world(home.path(), &root, &[]);
    let forge = forge(&w, PermissionMode::ReadOnly);
    let bugs = add_channel(&w, w.project, "bugs");
    let (window, ws) = open_tall(cx, home.path());
    w.post("@Forge fix it");
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);

    let row = format!("project-{}", w.project);
    wait_until(cx, "the project row", |cx| has(cx, window, row.clone()));
    click(cx, window, row.clone());
    wait_until(cx, "the overview", |cx| has(cx, window, "overview-page"));
    assert_eq!(
        label_of(cx, window, "overview-title").as_deref(),
        Some("demo")
    );
    assert_eq!(
        label_of(cx, window, "overview-path").as_deref(),
        Some(super::title_bar::home_relative(&root).as_str())
    );
    wait_label(cx, window, "overview-git".into(), "main · clean");
    assert!(!has(cx, window, "overview-remote"), "no origin");
    assert_eq!(
        label_of(cx, window, "overview-reveal").as_deref(),
        Some("Reveal in Finder")
    );
    assert_eq!(
        label_of(cx, window, "overview-new-channel").as_deref(),
        Some("New channel")
    );
    assert_eq!(
        label_of(cx, window, "title-project").as_deref(),
        Some("demo")
    );
    assert!(!has(cx, window, "title-channel"));

    // The channels table, this week.
    let general = w.channel;
    assert_eq!(
        label_of(cx, window, format!("overview-channel-{general}")).as_deref(),
        Some("#general")
    );
    assert_eq!(
        label_of(cx, window, format!("overview-channel-{bugs}")).as_deref(),
        Some("#bugs")
    );
    wait_label(
        cx,
        window,
        format!("overview-channel-{general}-threads"),
        "1",
    );
    assert_eq!(
        label_of(cx, window, format!("overview-channel-{general}-active")).as_deref(),
        Some("0")
    );
    assert_eq!(
        label_of(cx, window, format!("overview-channel-{bugs}-threads")).as_deref(),
        Some("0")
    );

    // Recent threads and the agents used here.
    let t = w.thread;
    assert_eq!(
        label_of(cx, window, format!("overview-thread-{t}")).as_deref(),
        Some("fix the login")
    );
    assert_eq!(
        label_of(cx, window, format!("overview-thread-{t}-status")).as_deref(),
        Some("Done")
    );
    assert_eq!(
        label_of(cx, window, format!("overview-thread-{t}-channel")).as_deref(),
        Some("#general")
    );
    assert_eq!(
        label_of(cx, window, format!("overview-thread-{t}-agents")).as_deref(),
        Some("Forge")
    );
    assert_eq!(
        label_of(cx, window, format!("overview-agent-{forge}")).as_deref(),
        Some("Forge")
    );
    assert_eq!(
        label_of(cx, window, format!("overview-agent-{forge}-runs")).as_deref(),
        Some("1")
    );
    assert!(!has(cx, window, "overview-terminal"), "no sessions");
    assert!(shows(
        cx,
        window,
        "Copied into every new worktree. Git ignores them, so agents would not see them otherwise."
    ));
    assert!(shows(
        cx,
        window,
        "Removes it from Crew. Your folder, branches and worktrees stay on disk."
    ));

    // Its row is the selected one; clicking it again folds it.
    let selected = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(row.clone()).and_then(|e| e.selected())
        })
        .unwrap();
    assert_eq!(selected, Some(true));
    assert!(has(cx, window, format!("channel-{bugs}")));
    click(cx, window, row.clone());
    assert!(!has(cx, window, format!("channel-{bugs}")), "folded");
    assert!(has(cx, window, "overview-page"), "the overview stays");
    click(cx, window, format!("project-chevron-{}", w.project));
    assert!(
        has(cx, window, format!("channel-{bugs}")),
        "the chevron opens it"
    );

    // Each row goes where it says.
    click(cx, window, format!("overview-thread-{t}"));
    wait_until(cx, "the thread", |cx| {
        ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(t))
    });
    assert!(!has(cx, window, "overview-page"));
    go(cx, window, &ws, Place::Project(w.project));
    wait_until(cx, "the overview", |cx| has(cx, window, "overview-page"));
    wait_until(cx, "the channel row", |cx| {
        has(cx, window, format!("overview-channel-{bugs}"))
    });
    click(cx, window, format!("overview-channel-{bugs}"));
    wait_until(cx, "bugs", |cx| {
        ws.read_with(cx, |ws, _| ws.nav.channel == Some(bugs))
    });
    go(cx, window, &ws, Place::Project(w.project));
    wait_until(cx, "the agent row", |cx| {
        has(cx, window, format!("overview-agent-{forge}"))
    });
    click(cx, window, format!("overview-agent-{forge}"));
    wait_until(cx, "the profile", |cx| has(cx, window, "agent-page"));

    // Reveal in Finder and New channel.
    go(cx, window, &ws, Place::Project(w.project));
    wait_until(cx, "the overview", |cx| has(cx, window, "overview-page"));
    let opened = record_opens(cx, &ws);
    click(cx, window, "overview-reveal");
    wait_until(cx, "Finder", |_| opened.lock().unwrap().len() == 1);
    assert_eq!(
        opened.lock().unwrap()[0],
        vec![OsString::from("-R"), root.clone().into_os_string()]
    );
    click(cx, window, "overview-new-channel");
    wait_until(cx, "the dialog", |cx| {
        shows(cx, window, "New channel in demo")
    });
    press(cx, window, "escape");

    // "All channels" lists the project's threads in the threads sidebar.
    wait_until(cx, "the recent threads", |cx| {
        has(cx, window, "overview-all-channels")
    });
    click(cx, window, "overview-all-channels");
    ws.read_with(cx, |ws, cx| {
        assert_eq!(
            super::sidebar::threads::sidebar_mode(cx),
            crate::prefs::SidebarMode::Threads
        );
        assert_eq!(
            crate::theme::scale::appearance(cx).sidebar_mode,
            crate::prefs::SidebarMode::Projects,
            "for now, not saved"
        );
        assert_eq!(ws.sidebar.threads.project, Some(w.project));
    });
    // Switching goes back to projects, still nothing saved.
    click(cx, window, "sidebar-switch");
    ws.read_with(cx, |_, cx| {
        assert_eq!(
            super::sidebar::threads::sidebar_mode(cx),
            crate::prefs::SidebarMode::Projects
        );
    });
}

#[gpui_kit::test]
fn setup_files_are_added_and_removed_from_the_overview(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    std::fs::write(root.join(".env"), "KEY=1\n").unwrap();
    std::fs::write(tmp.path().join("outside.txt"), "no").unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &root, &[]);
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the project", |cx| {
        has(cx, window, format!("project-{}", w.project))
    });

    // Setup files… opens the overview at its setup files.
    let row = format!("project-{}", w.project);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(row.clone(), cx);
    })
    .unwrap();
    wait_until(cx, "the menu", |cx| has(cx, window, "popup-menu"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(8usize, cx);
    })
    .unwrap();
    wait_until(cx, "the overview", |cx| has(cx, window, "overview-setup"));
    assert!(ws.read_with(cx, |ws, _| ws.settings.is_none()));
    assert!(!has(cx, window, "overview-setup-file-0"));

    // A file outside the folder is refused.
    let outside = tmp.path().join("outside.txt");
    ws.update(cx, |ws, _| {
        ws.set_file_picker(Rc::new(move |_| Task::ready(Some(outside.clone()))));
    });
    click(cx, window, "overview-setup-add");
    wait_until(cx, "the refusal", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.last_error.as_deref() == Some("Pick a file inside repo.")
        })
    });

    let env = root.join(".env");
    ws.update(cx, |ws, _| {
        ws.set_file_picker(Rc::new(move |_| Task::ready(Some(env.clone()))));
    });
    click(cx, window, "overview-setup-add");
    wait_label(cx, window, "overview-setup-file-0".into(), ".env");
    wait_label(cx, window, "overview-setup-file-0-size".into(), "6 B");
    let copied = |cx: &mut TestAppContext| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .project(w.project)
                .map(|p| p.copy_files.clone())
                .unwrap_or_default()
        })
    };
    assert_eq!(copied(cx), [".env"]);

    // The remove button shows on hover.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.hover("overview-setup-file-0", cx);
    })
    .unwrap();
    cx.run_until_parked();
    click(cx, window, "overview-setup-file-0-remove");
    wait_until(cx, "the file to go", |cx| {
        !has(cx, window, "overview-setup-file-0") && copied(cx).is_empty()
    });
}

#[gpui_kit::test]
fn remove_is_refused_while_an_agent_works_and_then_removes(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), busy_claude(tmp.path()));
    let w = world(home.path(), &root, &[]);
    let forge = forge(&w, PermissionMode::ReadOnly);
    add_channel(&w, w.project, "bugs");
    let (window, ws) = open_tall(cx, home.path());
    w.post("@Forge fix it");
    let run = run_of(cx, &ws, w.thread, forge, RunStatus::Working);
    go(cx, window, &ws, Place::Project(w.project));
    wait_until(cx, "the overview", |cx| has(cx, window, "overview-remove"));

    click(cx, window, "overview-remove");
    wait_until(cx, "the dialog", |cx| {
        shows(cx, window, "Remove demo from Crew?")
    });
    click(cx, window, "dialog-confirm");
    wait_label(
        cx,
        window,
        "dialog-error".into(),
        "Stop the agents in demo first.",
    );
    press(cx, window, "escape");
    let _: serde_json::Value = w.ask(Request::StopRun { run: run.id });
    run_of(cx, &ws, w.thread, forge, RunStatus::Stopped);

    click(cx, window, "overview-remove");
    wait_until(cx, "the dialog", |cx| {
        shows(cx, window, "Remove demo from Crew?")
    });
    click(cx, window, "dialog-confirm");
    wait_until(cx, "the project to go", |cx| {
        !has(cx, window, format!("project-{}", w.project))
    });
    assert!(!has(cx, window, "overview-page"));
    assert!(root.exists(), "nothing is deleted");
}

#[gpui_kit::test]
fn clicking_an_agent_opens_its_profile(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), editing_claude(tmp.path()));
    let w = world(home.path(), &root, &[]);
    let forge = forge(&w, PermissionMode::ReadOnly);
    let (window, ws) = open_tall(cx, home.path());
    w.post("@Forge fix it");
    let run = run_of(cx, &ws, w.thread, forge, RunStatus::Done);

    let row = format!("agent-{forge}");
    wait_until(cx, "the agent row", |cx| has(cx, window, row.clone()));
    click(cx, window, row.clone());
    wait_until(cx, "the profile", |cx| has(cx, window, "agent-page"));
    assert_eq!(label_of(cx, window, "agent-name").as_deref(), Some("Forge"));
    assert_eq!(
        label_of(cx, window, "agent-runtime").as_deref(),
        Some("claude · haiku")
    );
    assert_eq!(
        label_of(cx, window, "agent-status").as_deref(),
        Some("Idle")
    );
    wait_label(cx, window, "agent-role".into(), "Builds · in 1 project");
    assert_eq!(
        label_of(cx, window, "agent-start-thread").as_deref(),
        Some("Start a thread with Forge")
    );
    assert_eq!(
        label_of(cx, window, "agent-edit").as_deref(),
        Some("Edit agent")
    );
    assert_eq!(
        label_of(cx, window, "title-agents").as_deref(),
        Some("Agents")
    );
    assert_eq!(
        label_of(cx, window, "title-agent").as_deref(),
        Some("Forge")
    );

    // This week.
    wait_label(
        cx,
        window,
        "agent-stat-runs".into(),
        "Runs this week 1 · 0 last week",
    );
    assert_eq!(
        label_of(cx, window, "agent-stat-clean").as_deref(),
        Some("Finished cleanly 100% · 1 of 1")
    );
    assert!(
        label_of(cx, window, "agent-stat-avg")
            .is_some_and(|l| l.starts_with("Avg run time ") && l.ends_with('s'))
    );
    assert!(
        label_of(cx, window, "agent-stat-files")
            .is_some_and(|l| l.starts_with("Files changed ") && l.contains(" +"))
    );
    assert_eq!(
        label_of(cx, window, "agent-stat-handoffs").as_deref(),
        Some("Handoffs 0 sent · 0 received")
    );

    // Recent runs.
    let r = run.id;
    assert_eq!(
        label_of(cx, window, "agent-runs-strip").as_deref(),
        Some("Last 1 run: 1 done")
    );
    assert_eq!(
        label_of(cx, window, format!("agent-run-{r}-status")).as_deref(),
        Some("Done")
    );
    assert_eq!(
        label_of(cx, window, format!("agent-run-{r}-thread")).as_deref(),
        Some("fix the login")
    );
    assert_eq!(
        label_of(cx, window, format!("agent-run-{r}-where")).as_deref(),
        Some("demo · #general")
    );
    assert!(
        label_of(cx, window, format!("agent-run-{r}-files"))
            .is_some_and(|l| l.starts_with('+') && l.contains(" −"))
    );

    // Permissions and limits, and the threads it's in.
    assert_eq!(
        label_of(cx, window, "agent-limit-mode").as_deref(),
        Some("Read only (plan)")
    );
    assert_eq!(
        label_of(cx, window, "agent-limit-longest").as_deref(),
        Some("No limit")
    );
    assert_eq!(
        label_of(cx, window, "agent-limit-handoffs").as_deref(),
        Some("Channel default · 3 handoffs per thread")
    );
    assert!(!shows(cx, window, "Always allowed"));
    assert_eq!(
        label_of(cx, window, "agent-threads-count").as_deref(),
        Some("1 open")
    );
    assert_eq!(
        label_of(cx, window, format!("agent-thread-{}", w.thread)).as_deref(),
        Some("fix the login")
    );

    // The row is the selected one.
    let selected = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(row.clone()).and_then(|e| e.selected())
        })
        .unwrap();
    assert_eq!(selected, Some(true));

    // A run opens its thread.
    click(cx, window, format!("agent-run-{r}"));
    wait_until(cx, "the thread", |cx| {
        ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(w.thread))
    });

    // Edit agent and Change open Settings › Agents on it.
    go(cx, window, &ws, Place::Agent(forge));
    wait_until(cx, "the profile", |cx| has(cx, window, "agent-edit"));
    click(cx, window, "agent-edit");
    let editing = |cx: &mut TestAppContext| {
        ws.read_with(cx, |ws, _| {
            ws.settings.as_ref().is_some_and(|s| {
                s.page == super::settings::Page::Agents
                    && s.agents.form.as_ref().and_then(|f| f.editing) == Some(forge)
            })
        })
    };
    assert!(editing(cx));
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.close_settings(window, cx));
    })
    .unwrap();
    assert!(has(cx, window, "agent-page"), "back on the profile");
    click(cx, window, "agent-limits-change");
    assert!(editing(cx));
}

#[gpui_kit::test]
fn start_a_thread_opens_the_composer_in_its_last_channel(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), editing_claude(tmp.path()));
    let w = world(home.path(), &root, &[]);
    let forge = forge(&w, PermissionMode::ReadOnly);
    let bugs = add_channel(&w, w.project, "bugs");
    let there: ThreadId = w.ask(Request::CreateThread {
        channel: bugs,
        title: "in bugs".into(),
    });
    let (window, ws) = open_tall(cx, home.path());
    let _: crew_core::ids::MessageId = w.ask(Request::PostMessage {
        thread: there,
        body: "@Forge look".into(),
    });
    run_of(cx, &ws, there, forge, RunStatus::Done);

    go(cx, window, &ws, Place::Agent(forge));
    wait_until(cx, "the profile's runs", |cx| {
        has(cx, window, "agent-runs-strip")
    });
    click(cx, window, "agent-start-thread");
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.nav.channel, Some(bugs), "its last channel");
        assert!(ws.nav.drafting);
    });
    assert!(!has(cx, window, "agent-page"));
    assert_eq!(composer_text(cx, &ws), "@Forge ");
}

#[gpui_kit::test]
fn a_deleted_agent_and_a_removed_project_say_removed(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let other_root = tmp.path().join("other");
    repo_with_readme(&other_root);
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), editing_claude(tmp.path()));
    let w = world(home.path(), &root, &[]);
    let forge = forge(&w, PermissionMode::ReadOnly);
    let other: ProjectId = w.ask(Request::CreateProject {
        name: "other".into(),
        root: other_root.to_string_lossy().into_owned(),
        main_branch: "main".into(),
        work_mode: None,
        copy_files: vec![],
        setup_command: None,
    });
    let other_general = add_channel(&w, other, "general");
    let second: ThreadId = w.ask(Request::CreateThread {
        channel: other_general,
        title: "Second".into(),
    });
    let (window, ws) = open_tall(cx, home.path());
    w.post("@Forge fix it");
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);
    let _: crew_core::ids::MessageId = w.ask(Request::PostMessage {
        thread: second,
        body: "@Forge look".into(),
    });
    let gone = run_of(cx, &ws, second, forge, RunStatus::Done);
    let _: serde_json::Value = w.ask(Request::RemoveProject { project: other });
    wait_until(cx, "other to go", |cx| {
        ws.read_with(cx, |ws, _| ws.state().project(other).is_none())
    });

    go(cx, window, &ws, Place::Agent(forge));
    let r = gone.id;
    wait_label(
        cx,
        window,
        format!("agent-run-{r}-where"),
        "other (removed) · #general",
    );
    assert_eq!(
        label_of(cx, window, format!("agent-run-{r}-thread")).as_deref(),
        Some("Second")
    );

    let _: serde_json::Value = w.ask(Request::DeleteAgent { agent: forge });
    wait_until(cx, "the agent to go", |cx| {
        ws.read_with(cx, |ws, _| ws.state().agents().is_empty())
    });
    wait_label(cx, window, "agent-name".into(), "Forge (removed)");
    assert!(!has(cx, window, "agent-start-thread"));
    assert!(!has(cx, window, "agent-edit"));
    assert!(!has(cx, window, "agent-limits-change"));

    go(cx, window, &ws, Place::Project(w.project));
    wait_label(
        cx,
        window,
        format!("overview-agent-{forge}"),
        "Forge (removed)",
    );
    assert_eq!(
        label_of(cx, window, format!("overview-thread-{}-agents", w.thread)).as_deref(),
        Some("Forge (removed)")
    );
}

#[gpui_kit::test]
fn a_thread_with_a_worktree_opens_it_in_terminal(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), editing_claude(tmp.path()));
    let w = world(home.path(), &root, &[]);
    let forge = forge(&w, PermissionMode::EditFiles);
    let bare: ThreadId = w.ask(Request::CreateThread {
        channel: w.channel,
        title: "no worktree".into(),
    });
    let (window, ws) = open_tall(cx, home.path());
    w.post("@Forge fix it");
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);
    let path = ws.read_with(cx, |ws, _| {
        ws.state()
            .thread(w.thread)
            .and_then(|t| t.worktree.clone())
            .map(|wt| wt.path)
            .unwrap()
    });
    go(cx, window, &ws, Place::Channel(w.channel));
    let card = format!("thread-{}", w.thread);
    // Once the cards' counts are in, they stop changing under the pointer.
    let counts = format!("thread-counts-{}", w.thread);
    wait_until(cx, "the card's counts", |cx| {
        has(cx, window, format!("thread-branch-{}", w.thread))
            && label_of(cx, window, counts.clone()).is_some_and(|l| l.contains(" files "))
    });
    let opened = record_opens(cx, &ws);

    // A card redraws on hover and as crewd's counts come in; a press
    // that lands while it moves misses, so it presses again.
    let right_click = |cx: &mut TestAppContext, id: String| {
        wait_until(cx, "the menu", |cx| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.hover(id.clone(), cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.right_click(id.clone(), cx);
            })
            .unwrap();
            cx.run_until_parked();
            has(cx, window, "popup-menu")
        });
    };
    right_click(cx, format!("thread-{bare}"));
    assert!(!shows(cx, window, "Open worktree in terminal"));
    press(cx, window, "escape");

    right_click(cx, card);
    assert!(shows(cx, window, "Open worktree in terminal"));
    // Open, a separator, Rename, Copy branch name, then this.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(4usize, cx);
    })
    .unwrap();
    wait_until(cx, "Terminal", |_| opened.lock().unwrap().len() == 1);
    assert_eq!(
        opened.lock().unwrap()[0],
        vec![
            OsString::from("-a"),
            OsString::from("Terminal"),
            path.into_os_string()
        ]
    );
}

#[gpui_kit::test]
fn a_missing_folder_says_so_and_can_still_be_removed(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &root, &[]);
    std::fs::remove_dir_all(&root).unwrap();
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the project", |cx| {
        ws.read_with(cx, |ws, _| ws.state().project(w.project).is_some())
    });
    go(cx, window, &ws, Place::Project(w.project));
    wait_label(cx, window, "overview-missing".into(), "Folder not found");
    assert!(!has(cx, window, "overview-git"));

    click(cx, window, "overview-remove");
    wait_until(cx, "the dialog", |cx| {
        shows(cx, window, "Remove demo from Crew?")
    });
    click(cx, window, "dialog-confirm");
    wait_until(cx, "the project to go", |cx| {
        ws.read_with(cx, |ws, _| ws.state().project(w.project).is_none())
    });
    assert!(!has(cx, window, "overview-page"));
}
