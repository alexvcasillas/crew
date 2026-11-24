//! End to end: settings and the first launch, with a real crewd on a temp
//! home. The folder picker is replaced by one that answers a fixed path.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use crew_core::agent::{PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::policy::HandoffPolicy;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, Task, TestAppContext};

use super::first_launch::STARTER_NAME;
use super::settings::Page;
use super::settings::agents::{COMING_RUNTIMES, CREDENTIALS, INSTALL_CLAUDE};
use super::settings::projects::NOT_GIT;
use super::settings::runtimes::runtime_state;
use super::test_ui::{
    click, click_quickly, has, label_of, notifications, open_tall, press, shows, type_text,
    wait_to_show, wait_until,
};
use super::thread_tests::{live_run, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, fixture, recording_proxy, short_home};
use crew_proto::Request;

/// A git repository at `dir` on branch `main`.
fn git_repo(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q", "-b", "main"])
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init");
    dir.canonicalize().unwrap()
}

/// The next folder picked is `dir`.
fn picks(cx: &mut TestAppContext, ws: &Entity<Workspace>, dir: &Path) {
    let dir = dir.to_path_buf();
    ws.update(cx, |ws, _| {
        ws.set_folder_picker(Rc::new(move |_| Task::ready(Some(dir.clone()))));
    });
}

fn page(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> Option<Page> {
    ws.read_with(cx, |ws, _| ws.settings.as_ref().map(|s| s.page))
}

fn composer_focused(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
) -> bool {
    cx.update_window(window, |_, window, cx| {
        ws.read(cx).composer.read(cx).focused(window, cx)
    })
    .unwrap()
}

/// Selects all of the focused field's text and types `text` over it.
fn retype(cx: &mut TestAppContext, window: AnyWindowHandle, field: &str, text: &str) {
    click(cx, window, field.to_string());
    press(cx, window, "cmd-a backspace");
    type_text(cx, window, text);
}

#[gpui_kit::test]
fn first_launch_creates_project_agent_and_lands_in_general(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), "exit 0");
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let repo = git_repo(&tmp.path().join("demo"));

    let (window, ws) = open_tall(cx, home.path());
    wait_to_show(cx, window, "Welcome to Crew");
    assert!(has(cx, window, "first-launch-skip"));
    wait_until(cx, "the runtimes", |cx| {
        ws.read_with(cx, |ws, _| !ws.runtimes.is_empty())
    });
    let word = ws.read_with(cx, |ws, _| runtime_state(&ws.runtimes[0]).0);
    assert!(shows(cx, window, &format!("Claude Code 9.9.9 {word}")));

    picks(cx, &ws, &plain);
    click(cx, window, "first-launch-pick");
    wait_to_show(cx, window, NOT_GIT);
    ws.read_with(cx, |ws, _| assert!(ws.state().projects().is_empty()));

    picks(cx, &ws, &repo);
    click(cx, window, "first-launch-pick");
    wait_to_show(cx, window, "demo added");
    ws.read_with(cx, |ws, _| {
        let projects = ws.state().projects();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "demo");
        assert_eq!(projects[0].root, repo);
        assert_eq!(projects[0].main_branch, "main");
    });
    assert!(!shows(cx, window, NOT_GIT), "the error is gone");
    assert!(!has(cx, window, "first-launch-pick"), "step 1 is done");
    wait_until(cx, "#general with the project", |cx| {
        ws.read_with(cx, |ws, _| {
            let project = ws.state().projects()[0].id;
            ws.state().channels(project).len() == 1
        })
    });

    click(cx, window, "first-launch-starter");
    wait_until(cx, "the new thread in #general", |cx| {
        ws.read_with(cx, |ws, _| {
            let general = ws
                .nav
                .channel
                .and_then(|c| ws.state().channel(c))
                .is_some_and(|c| c.name == "general");
            general && ws.nav.drafting && ws.first_launch.is_none()
        })
    });
    ws.read_with(cx, |ws, _| {
        let agents = ws.state().agents();
        assert_eq!(agents.len(), 1);
        let spec = &agents[0].spec;
        assert_eq!(spec.name, STARTER_NAME);
        assert_eq!(spec.role, "Engineer");
        assert_eq!(spec.model, "sonnet");
        assert_eq!(spec.runtime, Runtime::ClaudeCode);
        assert_eq!(spec.permission_mode, PermissionMode::EditFiles);
        let project = ws.state().projects()[0].id;
        assert_eq!(ws.state().channels(project).len(), 1);
    });
    assert!(!has(cx, window, "first-launch"));
    assert!(shows(cx, window, "New thread"), "the new thread's composer");
    assert!(composer_focused(cx, window, &ws));
}

#[gpui_kit::test]
fn skipping_the_first_launch_opens_projects(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let (window, ws) = open_tall(cx, home.path());
    wait_to_show(cx, window, "Welcome to Crew");
    click(cx, window, "first-launch-skip");
    assert_eq!(page(cx, &ws), Some(Page::Projects));
    ws.read_with(cx, |ws, _| assert!(ws.first_launch.is_none()));
    press(cx, window, "cmd-1");
    assert_eq!(page(cx, &ws), None);
    assert!(!shows(cx, window, "Welcome to Crew"), "it stays skipped");
    assert!(shows(cx, window, "Nothing selected"));
}

#[gpui_kit::test]
fn editing_an_agent_changes_its_next_run(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let args = tmp.path().join("args.txt");
    let claude = fake_claude(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\ncat '{}'",
            args.display(),
            fixture("read_file.jsonl").display()
        ),
    );
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let forge = w.agents[0];
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the agent", |cx| {
        ws.read_with(cx, |ws, _| ws.state().agent(forge).is_some())
    });

    press(cx, window, "cmd-,");
    assert_eq!(page(cx, &ws), Some(Page::General));
    assert_eq!(
        label_of(cx, window, "settings-title").as_deref(),
        Some("Settings")
    );
    assert!(shows(cx, window, "4"), "max agents at once");
    click(cx, window, "settings-nav-agents");
    assert_eq!(
        label_of(cx, window, "agent-form-title").as_deref(),
        Some("Forge")
    );
    assert!(shows(cx, window, CREDENTIALS));
    assert!(
        shows(cx, window, "Read only (plan)"),
        "the permission it has"
    );

    click(cx, window, "agent-model-opus");
    assert!(shows(cx, window, "Opus (selected)"));
    click(cx, window, "agent-instructions");
    type_text(cx, window, "Keep answers short.");
    click(cx, window, "agent-tools");
    type_text(cx, window, "Bash(cargo test:*), Bash(git diff:*)");
    retype(cx, window, "agent-max-minutes", "45");
    click(cx, window, "agent-save");
    wait_to_show(cx, window, "Saved");
    ws.read_with(cx, |ws, _| {
        let spec = &ws.state().agent(forge).unwrap().spec;
        assert_eq!(spec.model, "opus");
        assert_eq!(spec.instructions, "Keep answers short.");
        assert_eq!(
            spec.allowed_tools,
            ["Bash(cargo test:*)", "Bash(git diff:*)"]
        );
        assert_eq!(spec.max_run_minutes, 45);
        assert_eq!(spec.permission_mode, PermissionMode::ReadOnly, "kept");
    });
    click(cx, window, "agent-role");
    type_text(cx, window, " lead");
    assert!(!shows(cx, window, "Saved"), "an edit since");

    w.post("@Forge look at the login");
    wait_until(cx, "the run's arguments", |_| {
        std::fs::read_to_string(&args).is_ok_and(|a| a.contains("--"))
    });
    let lines: Vec<String> = std::fs::read_to_string(&args)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let after = |flag: &str| {
        let i = lines
            .iter()
            .position(|l| l == flag)
            .unwrap_or_else(|| panic!("{flag} in {lines:?}"));
        lines[i + 1].clone()
    };
    assert_eq!(after("--model"), "opus");
    // The instructions follow crewd's Crew block in the same argument.
    assert!(after("--append-system-prompt").starts_with("You are Forge, "));
    let logged = std::fs::read_to_string(&args).unwrap();
    assert!(
        logged.contains("\n\nKeep answers short.\n--allowedTools\n"),
        "{logged}"
    );
    assert!(after("--allowedTools").contains("Bash(cargo test:*)"));

    press(cx, window, "escape");
    assert_eq!(page(cx, &ws), None, "Esc leaves settings");
}

#[gpui_kit::test]
fn coming_soon_runtimes_are_disabled(cx: &mut TestAppContext) {
    // No claude at all: crewd finds it isn't installed.
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let (window, ws) = open_tall(cx, home.path());
    wait_to_show(cx, window, "Welcome to Crew");
    wait_until(cx, "the runtimes", |cx| {
        ws.read_with(cx, |ws, _| !ws.runtimes.is_empty())
    });
    assert!(
        shows(cx, window, "Claude Code Not installed"),
        "the first launch's word"
    );

    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-agents");
    assert_eq!(page(cx, &ws), Some(Page::Agents));
    assert_eq!(
        label_of(cx, window, "agent-form-title").as_deref(),
        Some("New agent")
    );
    assert!(shows(cx, window, "Claude Code Not installed (selected)"));
    assert!(shows(cx, window, INSTALL_CLAUDE));
    for (key, name) in COMING_RUNTIMES {
        let id = format!("runtime-{key}");
        assert!(has(cx, window, id.clone()), "{name}");
        assert!(shows(cx, window, &format!("{name} Coming soon")));
        click(cx, window, id);
        assert!(shows(cx, window, &format!("{name} Coming soon")), "still");
    }

    click(cx, window, "agent-name");
    type_text(cx, window, "Scout");
    click(cx, window, "agent-save");
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    ws.read_with(cx, |ws, _| {
        assert!(ws.state().agents().is_empty(), "Save is off without claude");
    });
}

#[gpui_kit::test]
fn agent_form_checks_names_and_duplicates(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), "exit 0"));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the agent in the sidebar", |cx| {
        has(cx, window, format!("agent-{}", w.agents[0]))
    });
    wait_until(cx, "the runtimes", |cx| {
        ws.read_with(cx, |ws, _| !ws.runtimes.is_empty())
    });

    click(cx, window, "sidebar-agents-add");
    assert_eq!(page(cx, &ws), Some(Page::Agents));
    type_text(cx, window, "Scout bot");
    click(cx, window, "agent-save");
    let error = label_of(cx, window, "agent-error").unwrap();
    assert!(error.contains("letters, digits"), "{error}");

    retype(cx, window, "agent-name", "forge");
    click(cx, window, "agent-save");
    assert_eq!(
        label_of(cx, window, "agent-error").as_deref(),
        Some("You already have an agent called forge. Pick another name.")
    );

    retype(cx, window, "agent-name", "Scout");
    retype(cx, window, "agent-role", "Reviewer");
    click(cx, window, "agent-model-haiku");
    click(cx, window, "agent-policy-budget");
    click(cx, window, "agent-save");
    wait_until(cx, "Scout", |cx| {
        ws.read_with(cx, |ws, _| ws.state().agents().len() == 2)
    });
    assert!(!has(cx, window, "agent-error"));
    let scout = ws.read_with(cx, |ws, _| {
        let a = ws
            .state()
            .agents()
            .into_iter()
            .find(|a| a.spec.name == "Scout")
            .unwrap();
        assert_eq!(a.spec.role, "Reviewer");
        assert_eq!(a.spec.model, "haiku");
        assert_eq!(a.spec.handoff_policy, Some(HandoffPolicy::Budget));
        a.id
    });
    wait_until(cx, "the form to edit Scout", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.settings
                .as_ref()
                .unwrap()
                .agents
                .form
                .as_ref()
                .unwrap()
                .editing
                == Some(scout)
        })
    });

    click(cx, window, "agent-duplicate");
    assert_eq!(
        label_of(cx, window, "agent-form-title").as_deref(),
        Some("Scout-copy")
    );
    click(cx, window, "agent-save");
    wait_until(cx, "the copy", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().agents().iter().any(|a| {
                a.spec.name == "Scout-copy" && a.spec.model == "haiku" && a.spec.role == "Reviewer"
            })
        })
    });

    // Delete Scout, after confirming.
    click(cx, window, format!("settings-agent-{scout}"));
    click(cx, window, "agent-delete");
    assert!(has(cx, window, "delete-agent-ask"));
    click(cx, window, "delete-agent-confirm");
    wait_until(cx, "Scout deleted", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().agents().iter().all(|a| a.id != scout)
        })
    });
}

#[gpui_kit::test]
fn deleting_a_busy_agent_shows_crewds_reason(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = fake_claude(tmp.path(), "exec sleep 60");
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_tall(cx, home.path());
    w.post("@Forge go");
    let run = live_run(cx, &ws, w.thread);
    wait_until(cx, "the run to work", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .run(run)
                .is_some_and(|r| r.status == RunStatus::Working)
        })
    });

    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-agents");
    click(cx, window, "agent-delete");
    click(cx, window, "delete-agent-confirm");
    wait_until(cx, "the refusal", |cx| notifications(cx, window) > 0);
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.last_error.as_deref(), Some("Stop Forge's runs first."));
        assert_eq!(ws.state().agents().len(), 1);
    });
    press(cx, window, "cmd-shift-.");
}

#[gpui_kit::test]
fn project_setup_command_is_saved(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the project", |cx| {
        ws.read_with(cx, |ws, _| ws.state().project(w.project).is_some())
    });

    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-projects");
    assert_eq!(
        label_of(cx, window, "project-form-title").as_deref(),
        Some("demo")
    );
    click(cx, window, "project-setup");
    type_text(cx, window, "npm ci");
    click(cx, window, "project-copy-files");
    type_text(cx, window, ".env");
    press(cx, window, "enter");
    type_text(cx, window, "config/local.toml");
    click(cx, window, "project-save");
    wait_until(cx, "the setup saved", |cx| {
        ws.read_with(cx, |ws, _| {
            let p = ws.state().project(w.project).unwrap();
            p.setup_command.as_deref() == Some("npm ci")
                && p.copy_files == [".env", "config/local.toml"]
        })
    });
    // crewd's answer and its project update arrive apart.
    wait_to_show(cx, window, "Saved");

    // Channels: add one, and set #general's handoffs.
    click(cx, window, "project-new-channel");
    type_text(cx, window, "#bugs");
    click(cx, window, "project-add-channel");
    wait_until(cx, "#bugs", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .channels(w.project)
                .iter()
                .any(|c| c.name == "bugs")
        })
    });
    // Quick clicks, without waiting for crewd, build on each other.
    let up = format!("channel-budget-up-{}", w.channel);
    click_quickly(
        cx,
        window,
        &[
            format!("channel-policy-{}-budget", w.channel).into(),
            up.clone().into(),
            up.into(),
        ],
    );
    assert_eq!(
        label_of(cx, window, format!("channel-budget-{}", w.channel)).as_deref(),
        Some("5")
    );
    wait_until(cx, "#general's handoffs", |cx| {
        ws.read_with(cx, |ws, _| {
            let c = ws.state().channel(w.channel).unwrap();
            c.handoff_policy == Some(HandoffPolicy::Budget) && c.handoff_budget == Some(5)
        })
    });
}

#[gpui_kit::test]
fn adding_a_project_needs_a_git_folder(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let shop = git_repo(&tmp.path().join("shop"));
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the project", |cx| {
        ws.read_with(cx, |ws, _| ws.state().project(w.project).is_some())
    });

    picks(cx, &ws, &plain);
    click(cx, window, "sidebar-projects-add");
    assert_eq!(page(cx, &ws), Some(Page::Projects));
    wait_to_show(cx, window, NOT_GIT);

    picks(cx, &ws, &shop);
    click(cx, window, "settings-add-project");
    wait_until(cx, "the add form", |cx| has(cx, window, "project-adding"));
    assert_eq!(
        label_of(cx, window, "project-add-branch").as_deref(),
        Some("main")
    );
    retype(cx, window, "project-add-name", "Shop");
    click(cx, window, "project-add-confirm");
    wait_until(cx, "the new project", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .projects()
                .iter()
                .any(|p| p.name == "Shop" && p.root == shop && p.main_branch == "main")
        })
    });
    wait_until(cx, "its form", |cx| {
        label_of(cx, window, "project-form-title").as_deref() == Some("Shop")
    });
    wait_until(cx, "Shop's #general", |cx| {
        ws.read_with(cx, |ws, _| {
            let shop = ws
                .state()
                .projects()
                .into_iter()
                .find(|p| p.name == "Shop")
                .unwrap()
                .id;
            let names: Vec<String> = ws
                .state()
                .channels(shop)
                .iter()
                .map(|c| c.name.clone())
                .collect();
            names == ["general"]
        })
    });

    // The same folder again.
    click(cx, window, "settings-add-project");
    wait_to_show(
        cx,
        window,
        "shop is already a project. Open it from the sidebar.",
    );
}

#[gpui_kit::test]
fn settings_open_from_the_title_bar_and_list_shortcuts(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let crewd_home = short_home();
    let _daemon = Daemon::start(crewd_home.path().to_path_buf());
    let w = world(crewd_home.path(), &tmp.path().join("repo"), &[]);
    let home = short_home();
    let seen = recording_proxy(
        &home.path().join("crewd.sock"),
        crewd_home.path().join("crewd.sock"),
    );
    let asks = || {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, Request::Runtimes))
            .count()
    };
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the project", |cx| {
        ws.read_with(cx, |ws, _| ws.state().project(w.project).is_some())
    });
    click(cx, window, "title-settings");
    assert_eq!(page(cx, &ws), Some(Page::General));
    click(cx, window, "settings-nav-shortcuts");
    assert!(shows(cx, window, "Stop all runs ⌘⇧."));
    assert!(shows(cx, window, "Settings ⌘,"));
    click(cx, window, "settings-nav-runtimes");
    wait_to_show(cx, window, "Claude Code Not installed");
    // No codex in this test's crewd; Codex is no longer "Coming soon".
    assert!(shows(cx, window, "Codex Not installed"));
    assert!(!shows(cx, window, "Codex Coming soon"));
    wait_until(cx, "the first ask to end", |cx| {
        ws.read_with(cx, |ws, _| !ws.runtimes_checking)
    });
    let before = asks();
    assert!(before >= 1, "asked on connect");
    click(cx, window, "runtimes-refresh");
    wait_until(cx, "a second ask", |_| asks() == before + 1);
    wait_until(cx, "its answer", |cx| {
        ws.read_with(cx, |ws, _| !ws.runtimes_checking)
    });
    assert!(shows(cx, window, "Claude Code Not installed"));
    click(cx, window, "settings-back");
    assert_eq!(page(cx, &ws), None);
}

/// A picker that waits until the test answers, counting how often it was
/// opened.
struct SlowPicker {
    opened: Rc<std::cell::Cell<usize>>,
    answer: Rc<std::cell::RefCell<Option<futures::channel::oneshot::Sender<PathBuf>>>>,
}

fn slow_picker(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> SlowPicker {
    let opened = Rc::new(std::cell::Cell::new(0));
    let answer = Rc::new(std::cell::RefCell::new(None));
    let (o, a) = (opened.clone(), answer.clone());
    ws.update(cx, |ws, _| {
        ws.set_folder_picker(Rc::new(move |cx: &mut gpui_kit::App| {
            o.set(o.get() + 1);
            let (tx, rx) = futures::channel::oneshot::channel();
            *a.borrow_mut() = Some(tx);
            cx.spawn(async move |_| rx.await.ok())
        }));
    });
    SlowPicker { opened, answer }
}

#[gpui_kit::test]
fn choosing_a_folder_twice_adds_one_project(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let repo = git_repo(&tmp.path().join("demo"));
    let (window, ws) = open_tall(cx, home.path());
    wait_to_show(cx, window, "Welcome to Crew");
    let picker = slow_picker(cx, &ws);

    click(cx, window, "first-launch-pick");
    click(cx, window, "first-launch-pick");
    assert_eq!(picker.opened.get(), 1, "one picker at a time");
    picker
        .answer
        .borrow_mut()
        .take()
        .unwrap()
        .send(repo.clone())
        .unwrap();
    wait_to_show(cx, window, "demo added");
    // Nothing to pick any more, and a late second answer adds nothing.
    assert!(!has(cx, window, "first-launch-pick"));
    ws.update(cx, |ws, _| {
        ws.set_folder_picker(Rc::new(move |_| Task::ready(Some(repo.clone()))));
    });
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.choose_folder(
                super::settings::projects::FolderFor::FirstLaunch,
                window,
                cx,
            )
        })
    })
    .unwrap();
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    ws.read_with(cx, |ws, _| assert_eq!(ws.state().projects().len(), 1));

    // Settings' Add project is off while a picker is open.
    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-projects");
    let picker = slow_picker(cx, &ws);
    click(cx, window, "settings-add-project");
    click(cx, window, "settings-add-project");
    assert_eq!(picker.opened.get(), 1);
    drop(picker.answer.borrow_mut().take());
    wait_until(cx, "the picker to close", |cx| {
        ws.read_with(cx, |ws, _| ws.folder_task.is_none())
    });
}

#[gpui_kit::test]
fn a_late_save_lands_only_on_its_own_form(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), "exit 0"));
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
    click(cx, window, "settings-new-agent");
    type_text(cx, window, "Scout");
    // Save the new agent, then switch to Forge before crewd answers.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.save_agent(window, cx);
            ws.edit_agent(forge, window, cx);
        })
    })
    .unwrap();
    wait_until(cx, "Scout", |cx| {
        ws.read_with(cx, |ws, _| ws.state().agents().len() == 2)
    });
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    ws.read_with(cx, |ws, _| {
        let form = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        assert_eq!(form.editing, Some(forge));
        assert!(!form.saved, "Scout's save isn't Forge's");
    });
}

#[gpui_kit::test]
fn duplicate_copies_what_you_typed_and_permission_changes(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), "exit 0"));
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

    // The permission dropdown.
    click(cx, window, "agent-permission");
    assert!(
        shows(cx, window, "Edit files, ask before commands"),
        "the menu"
    );
    cx.update_window(window, |_, window, cx| {
        use gpui_kit::test::TestWindowExt as _;
        window.render_frame(cx);
        window.within("popup-menu").click(0usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    ws.read_with(cx, |ws, _| {
        let form = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        assert_eq!(form.permission, PermissionMode::EditFiles);
    });

    // An invalid longest run is copied as typed.
    retype(cx, window, "agent-max-minutes", "0");
    click(cx, window, "agent-duplicate");
    assert_eq!(
        label_of(cx, window, "agent-form-title").as_deref(),
        Some("Forge-copy")
    );
    ws.read_with(cx, |ws, cx| {
        let form = ws.settings.as_ref().unwrap().agents.form.as_ref().unwrap();
        assert_eq!(form.editing, None);
        assert_eq!(form.max_minutes.read(cx).value().as_ref(), "0");
        assert_eq!(form.permission, PermissionMode::EditFiles);
        assert_eq!(form.role.read(cx).value().as_ref(), "Forge things");
    });
}

#[gpui_kit::test]
fn slow_runtimes_fill_in_later(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path().join("claude-slow.sh");
    std::fs::write(
        &claude,
        "#!/bin/sh\nsleep 4\ncase \"$1\" in\n  --version) echo '9.9.9 (Claude Code)';;\nesac\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = short_home();
    // crewd answers with what it found after 100 ms; each CLI call takes 4 s.
    let _daemon = Daemon::start_with_options(
        home.path().to_path_buf(),
        crewd::ServeOptions {
            claude_bin: Some(claude),
            runtimes_wait: Some(std::time::Duration::from_millis(100)),
            ..Default::default()
        },
    );
    let (window, ws) = open_tall(cx, home.path());
    wait_to_show(cx, window, "Welcome to Crew");
    let started = std::time::Instant::now();
    wait_to_show(cx, window, "Claude Code Installed");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(3500),
        "found before the CLI answered: {:?}",
        started.elapsed()
    );
    wait_to_show(cx, window, "Claude Code 9.9.9 Signed in");
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.runtimes[0].version.as_deref(), Some("9.9.9"));
    });
}
