//! End to end: the sidebar's Projects form (groups, pins, the filter and
//! its menu, single-channel rows, duplicate names, the project menu and
//! its dialogs), with a real crewd on a temp home.

use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crew_core::ids::{ChannelId, ProjectId};
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::test_ui::{
    bounds_of, click, description_of, has, label_of, open, press, shows, shows_within, type_text,
    wait_until,
};
use super::thread_tests::{World, world};
use super::workspace::Workspace;
use crate::prefs::{ShowProjects, SidebarPrefs, SortProjects};
use crate::test_support::{Daemon, short_home};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn add_project(w: &World, name: &str, root: &Path) -> ProjectId {
    std::fs::create_dir_all(root).unwrap();
    w.ask(Request::CreateProject {
        name: name.into(),
        root: root.to_string_lossy().into_owned(),
        main_branch: "main".into(),
        work_mode: None,
        copy_files: vec![],
        setup_command: None,
    })
}

fn add_channel(w: &World, project: ProjectId, name: &str) -> ChannelId {
    w.ask(Request::CreateChannel {
        project,
        name: name.into(),
    })
}

/// A thread in `channel`, which makes its project active.
fn add_thread(w: &World, channel: ChannelId) {
    let _: crew_core::ids::ThreadId = w.ask(Request::CreateThread {
        channel,
        title: "a thread".into(),
    });
}

fn project_row(project: ProjectId) -> String {
    format!("project-{project}")
}

fn wait_has(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) {
    wait_until(cx, &id.clone(), |cx| has(cx, window, id.clone()));
}

fn top(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) -> f32 {
    let bounds = bounds_of(cx, window, id.clone()).unwrap_or_else(|| panic!("no {id}"));
    f32::from(bounds.origin.y)
}

fn is_selected(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) -> bool {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).and_then(|e| e.selected()) == Some(true)
    })
    .unwrap()
}

fn right_click(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
    wait_until(cx, "the menu", |cx| has(cx, window, "popup-menu"));
}

/// Clicks the `ix`th item of the open menu, separators counted.
fn menu_click(cx: &mut TestAppContext, window: AnyWindowHandle, ix: usize) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(ix, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn focus_filter(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.sidebar
                .filter
                .update(cx, |input, cx| input.focus(window, cx));
        });
    })
    .unwrap();
    cx.run_until_parked();
}

fn set_now(cx: &mut TestAppContext, ws: &Entity<Workspace>, now: i64) {
    ws.update(cx, |ws, cx| {
        ws.sidebar.now = Some(now);
        cx.notify();
    });
    cx.run_until_parked();
}

fn shown_channel(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> Option<ChannelId> {
    ws.read_with(cx, |ws, _| ws.nav.channel)
}

#[gpui_kit::test]
fn projects_group_into_pinned_active_and_other(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let api = add_project(&w, "api", &tmp.path().join("api"));
    let api_general = add_channel(&w, api, "general");
    let (window, ws) = open(cx, home.path());
    // api has no threads: it isn't active, so it waits in Other.
    wait_until(cx, "Other projects", |cx| {
        label_of(cx, window, "sidebar-other").as_deref() == Some("Other projects · 1")
    });
    assert_eq!(
        label_of(cx, window, "sidebar-group-active").as_deref(),
        Some("Active")
    );
    assert!(has(cx, window, project_row(w.project)));
    assert!(!has(cx, window, project_row(api)), "Other starts closed");
    assert!(!has(cx, window, "sidebar-group-pinned"));
    click(cx, window, "sidebar-other");
    wait_has(cx, window, project_row(api));
    // Pin to top: the project moves into Pinned, above Active.
    right_click(cx, window, project_row(api));
    assert!(shows_within(cx, window, "popup-menu", "Pin to top"));
    menu_click(cx, window, 1);
    wait_has(cx, window, "sidebar-group-pinned".into());
    assert_eq!(
        label_of(cx, window, "sidebar-group-pinned").as_deref(),
        Some("Pinned")
    );
    let pinned = top(cx, window, "sidebar-group-pinned".into());
    let api_y = top(cx, window, project_row(api));
    let active = top(cx, window, "sidebar-group-active".into());
    let demo_y = top(cx, window, project_row(w.project));
    assert!(pinned < api_y && api_y < active && active < demo_y);
    assert!(!has(cx, window, "sidebar-other"));

    // A week later the unpinned project is quiet: Other projects, which
    // stays open as you left it.
    let now = crate::ui::common::now_ms() + 8 * DAY_MS;
    set_now(cx, &ws, now);
    assert_eq!(
        label_of(cx, window, "sidebar-other").as_deref(),
        Some("Other projects · 1")
    );
    assert!(!has(cx, window, "sidebar-group-active"));
    assert!(has(cx, window, project_row(w.project)));
    assert!(has(cx, window, project_row(api)), "pinned stays");
    click(cx, window, "sidebar-other");
    assert!(!has(cx, window, project_row(w.project)));
    click(cx, window, "sidebar-other");
    assert!(has(cx, window, project_row(w.project)));
    click(cx, window, "sidebar-other");

    // Unpin: it joins the others.
    right_click(cx, window, project_row(api));
    assert!(shows_within(cx, window, "popup-menu", "Unpin"));
    menu_click(cx, window, 1);
    wait_until(cx, "the pin to go", |cx| {
        label_of(cx, window, "sidebar-other").as_deref() == Some("Other projects · 2")
    });
    assert!(!has(cx, window, "sidebar-group-pinned"));

    // Opening a channel in closed Other opens Other so it shows selected.
    assert!(!has(cx, window, project_row(api)));
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.open_place(super::workspace::Place::Channel(api_general), window, cx)
        });
    })
    .unwrap();
    cx.run_until_parked();
    wait_has(cx, window, project_row(api));
    assert!(is_selected(cx, window, project_row(api)));

    // Pinned only still lists the open channel's project.
    click(cx, window, "sidebar-filter-options");
    click(cx, window, "sidebar-show-pinned");
    press(cx, window, "escape");
    assert!(has(cx, window, project_row(api)));
    assert!(!has(cx, window, project_row(w.project)));
}

#[gpui_kit::test]
fn a_single_channel_project_is_one_row_that_opens_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    wait_has(cx, window, project_row(w.project));
    assert!(!has(cx, window, format!("channel-{}", w.channel)));
    // New thread with nothing open starts one in the first channel.
    click(cx, window, "sidebar-new-thread");
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.nav.channel, Some(w.channel));
        assert!(ws.nav.drafting);
    });
    click(cx, window, project_row(w.project));
    wait_until(cx, "general to open", |cx| {
        shown_channel(cx, &ws) == Some(w.channel) && has(cx, window, "channel-view")
    });
    assert!(is_selected(cx, window, project_row(w.project)));
    // It stays selected while a thread in it is open.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.open_place(super::workspace::Place::Thread(w.thread), window, cx)
        });
    })
    .unwrap();
    wait_until(cx, "the thread", |cx| {
        ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(w.thread))
    });
    assert!(is_selected(cx, window, project_row(w.project)));

    // A second channel: the project opens out into its channels.
    let bugs = add_channel(&w, w.project, "bugs");
    wait_has(cx, window, format!("channel-{bugs}"));
    assert!(has(cx, window, format!("channel-{}", w.channel)));
    assert!(is_selected(cx, window, format!("channel-{}", w.channel)));
    // Open, the project row carries no status of its own.
    assert_eq!(
        label_of(cx, window, format!("project-status-{}", w.project)).as_deref(),
        Some("")
    );
    click(cx, window, project_row(w.project));
    assert!(!has(cx, window, format!("channel-{bugs}")), "it closes");
}

#[gpui_kit::test]
fn the_filter_finds_projects_and_channels_and_keys_move_through_them(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let supa = add_project(&w, "supa-app", &tmp.path().join("supa"));
    let supa_general = add_channel(&w, supa, "general");
    add_thread(&w, supa_general);
    let api = add_project(&w, "api", &tmp.path().join("api"));
    let api_general = add_channel(&w, api, "general");
    add_thread(&w, api_general);
    let support = add_channel(&w, api, "support");
    let (window, ws) = open(cx, home.path());
    wait_has(cx, window, project_row(api));
    assert!(has(cx, window, format!("agent-{}", w.agents[0])));
    assert_eq!(
        label_of(cx, window, "sidebar-switch").as_deref(),
        Some("Show threads")
    );
    assert_eq!(
        description_of(cx, window, "sidebar-switch").as_deref(),
        Some("Coming soon.")
    );

    focus_filter(cx, window, &ws);
    type_text(cx, window, "SUP");
    wait_until(cx, "the results", |cx| {
        label_of(cx, window, "sidebar-group-projects").as_deref() == Some("Projects · 1")
    });
    assert_eq!(
        label_of(cx, window, "sidebar-group-channels").as_deref(),
        Some("Channels · 1")
    );
    assert_eq!(
        description_of(cx, window, format!("project-name-{supa}")).as_deref(),
        Some("Matches “sup”")
    );
    assert_eq!(
        label_of(cx, window, format!("channel-hit-{support}")).as_deref(),
        Some("#support in api")
    );
    assert!(!has(cx, window, project_row(w.project)), "no match");
    assert!(!has(cx, window, format!("agent-{}", w.agents[0])));
    assert!(shows(cx, window, "↓ results, ↵ open, esc clear"));
    assert!(has(cx, window, "sidebar-filter-clear"));
    assert!(
        is_selected(cx, window, project_row(supa)),
        "the first result"
    );

    press(cx, window, "down");
    assert!(is_selected(cx, window, format!("channel-hit-{support}")));
    assert!(!is_selected(cx, window, project_row(supa)));
    press(cx, window, "up");
    assert!(is_selected(cx, window, project_row(supa)));
    press(cx, window, "enter");
    wait_until(cx, "supa-app's channel", |cx| {
        shown_channel(cx, &ws) == Some(supa_general)
    });
    press(cx, window, "down enter");
    wait_until(cx, "#support", |cx| shown_channel(cx, &ws) == Some(support));

    // esc empties the filter: the groups come back.
    press(cx, window, "escape");
    wait_has(cx, window, "sidebar-group-active".into());
    assert!(!has(cx, window, "sidebar-group-projects"));
    assert!(has(cx, window, project_row(w.project)));
    assert!(!shows(cx, window, "↓ results, ↵ open, esc clear"));

    type_text(cx, window, "nothing like it");
    wait_has(cx, window, "sidebar-no-matches".into());
    click(cx, window, "sidebar-filter-clear");
    wait_has(cx, window, project_row(w.project));
}

#[gpui_kit::test]
fn the_filter_menu_choices_apply_and_are_kept(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let alpha = add_project(&w, "alpha", &tmp.path().join("alpha"));
    let alpha_general = add_channel(&w, alpha, "general");
    add_thread(&w, alpha_general);
    let (window, _ws) = open(cx, home.path());
    wait_has(cx, window, project_row(alpha));
    // Recent activity first: alpha is newer.
    assert!(top(cx, window, project_row(alpha)) < top(cx, window, project_row(w.project)));

    click(cx, window, "sidebar-filter-options");
    assert!(has(cx, window, "sidebar-filter-menu"));
    for label in [
        "All projects",
        "Active only",
        "Pinned only",
        "Recent activity",
        "Name",
        "Show agents",
    ] {
        assert!(
            shows_within(cx, window, "sidebar-filter-menu", label),
            "{label}"
        );
    }
    assert!(is_selected(cx, window, "sidebar-show-all".into()));
    assert!(is_selected(cx, window, "sidebar-sort-recent".into()));

    click(cx, window, "sidebar-sort-name");
    assert!(is_selected(cx, window, "sidebar-sort-name".into()));
    assert!(top(cx, window, project_row(alpha)) < top(cx, window, project_row(w.project)));
    // "demo" before "zeta" by name, though zeta is newer.
    let _: serde_json::Value = w.ask(Request::RenameProject {
        project: alpha,
        name: "zeta".into(),
    });
    wait_until(cx, "the rename", |cx| {
        label_of(cx, window, project_row(alpha)).as_deref() == Some("zeta")
    });
    assert!(top(cx, window, project_row(w.project)) < top(cx, window, project_row(alpha)));

    click(cx, window, "sidebar-show-agents");
    assert!(!has(cx, window, format!("agent-{}", w.agents[0])));
    assert!(!has(cx, window, "sidebar-agents"));
    click(cx, window, "sidebar-show-pinned");
    assert!(!has(cx, window, project_row(w.project)));
    assert!(!has(cx, window, "sidebar-group-active"));

    // esc closes the menu.
    press(cx, window, "escape");
    assert!(!has(cx, window, "sidebar-filter-menu"));

    let want = SidebarPrefs {
        show: ShowProjects::Pinned,
        sort: SortProjects::Name,
        agents: false,
    };
    wait_until(cx, "the prefs to be saved", |_| {
        SidebarPrefs::load(home.path()) == want
    });
    // A new window starts with them.
    let (window2, ws2) = open(cx, home.path());
    wait_until(cx, "the second window", |cx| {
        ws2.read_with(cx, |ws, _| ws.state().project(alpha).is_some())
    });
    assert_eq!(ws2.read_with(cx, |ws, _| ws.sidebar.prefs), want);
    assert!(!has(cx, window2, project_row(alpha)));

    // Clicking outside closes it; the button opens and closes it.
    click(cx, window, "sidebar-filter-options");
    assert!(has(cx, window, "sidebar-filter-menu"));
    click(cx, window, "sidebar-filter-options");
    assert!(!has(cx, window, "sidebar-filter-menu"));
    click(cx, window, "sidebar-filter-options");
    click(cx, window, "status-bar");
    assert!(!has(cx, window, "sidebar-filter-menu"));
    // Window shortcuts still answer: ⌘K opens the palette.
    press(cx, window, "cmd-k");
    wait_until(cx, "the palette", |cx| has(cx, window, "palette"));
}

#[gpui_kit::test]
fn clashing_project_names_get_a_path_hint(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("a").join("demo"), &[]);
    let twin = add_project(&w, "demo", &tmp.path().join("b").join("demo"));
    let other = add_project(&w, "web", &tmp.path().join("web"));
    for project in [twin, other] {
        add_thread(&w, add_channel(&w, project, "general"));
    }
    let (window, _ws) = open(cx, home.path());
    wait_has(cx, window, project_row(twin));
    wait_has(cx, window, project_row(other));
    // The folder each sits in, cut from the left to three folders.
    let names: Vec<String> = tmp
        .path()
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let end = names[names.len() - 2..].join("/");
    assert_eq!(
        label_of(cx, window, format!("project-hint-{}", w.project)),
        Some(format!("· …/{end}/a"))
    );
    assert_eq!(
        label_of(cx, window, format!("project-hint-{twin}")),
        Some(format!("· …/{end}/b"))
    );
    assert!(!has(cx, window, format!("project-hint-{other}")));
}

/// Swaps the `open` launcher for one that records its arguments.
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

#[gpui_kit::test]
fn the_project_menu_reveals_opens_copies_renames_and_removes(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let root = tmp.path().join("repo");
    let w = world(home.path(), &root, &[]);
    let (window, ws) = open(cx, home.path());
    wait_has(cx, window, project_row(w.project));
    let opened = record_opens(cx, &ws);

    right_click(cx, window, project_row(w.project));
    for label in [
        "Project overview",
        "Pin to top",
        "New thread in #general",
        "New channel…",
        "Reveal in Finder",
        "Open in terminal",
        "Copy path",
        "Setup files…",
        "Rename…",
        "Remove project…",
    ] {
        assert!(shows_within(cx, window, "popup-menu", label), "{label}");
    }
    menu_click(cx, window, 5);
    right_click(cx, window, project_row(w.project));
    menu_click(cx, window, 6);
    let root_arg: OsString = root.clone().into_os_string();
    wait_until(cx, "the launches", |_| opened.lock().unwrap().len() == 2);
    assert_eq!(
        *opened.lock().unwrap(),
        vec![
            vec![OsString::from("-R"), root_arg.clone()],
            vec!["-a".into(), "Terminal".into(), root_arg],
        ]
    );

    right_click(cx, window, project_row(w.project));
    menu_click(cx, window, 7);
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()),
        Some(root.to_string_lossy().into_owned())
    );

    // Project overview opens its first channel for now.
    right_click(cx, window, project_row(w.project));
    menu_click(cx, window, 0);
    wait_until(cx, "general", |cx| {
        shown_channel(cx, &ws) == Some(w.channel)
    });

    // New thread in #general.
    right_click(cx, window, project_row(w.project));
    menu_click(cx, window, 2);
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.nav.channel, Some(w.channel));
        assert!(ws.nav.drafting);
    });

    // Setup files: Settings › Projects on this project.
    right_click(cx, window, project_row(w.project));
    menu_click(cx, window, 8);
    ws.read_with(cx, |ws, _| {
        let settings = ws.settings.as_ref().expect("settings");
        assert_eq!(settings.page, super::settings::Page::Projects);
        assert_eq!(settings.projects.selected, Some(w.project));
    });
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.close_settings(window, cx));
    })
    .unwrap();

    // Rename.
    right_click(cx, window, project_row(w.project));
    menu_click(cx, window, 10);
    wait_until(cx, "the rename dialog", |cx| {
        shows(cx, window, "Rename project")
    });
    press(cx, window, "cmd-a backspace");
    press(cx, window, "enter");
    wait_until(cx, "the error", |cx| {
        label_of(cx, window, "dialog-error").as_deref() == Some("Give the project a name.")
    });
    type_text(cx, window, "Demo app");
    press(cx, window, "enter");
    wait_until(cx, "the new name", |cx| {
        label_of(cx, window, project_row(w.project)).as_deref() == Some("Demo app")
    });
    assert!(ws.read_with(cx, |ws, _| ws.dialog.is_none()));

    // Remove asks first, then the project leaves.
    right_click(cx, window, project_row(w.project));
    menu_click(cx, window, 11);
    wait_until(cx, "the remove dialog", |cx| {
        shows(cx, window, "Remove Demo app from Crew?")
    });
    assert!(shows(
        cx,
        window,
        "Removes it from Crew. Your folder, branches and worktrees stay on disk."
    ));
    click(cx, window, "dialog-confirm");
    wait_until(cx, "the project to go", |cx| {
        !has(cx, window, project_row(w.project))
    });
    assert!(root.exists(), "nothing is deleted");
}

#[gpui_kit::test]
fn new_channel_checks_the_name_then_creates_and_opens_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    wait_has(cx, window, project_row(w.project));

    right_click(cx, window, project_row(w.project));
    menu_click(cx, window, 3);
    wait_until(cx, "the dialog", |cx| {
        shows(cx, window, "New channel in demo")
    });
    assert!(shows(
        cx,
        window,
        "Lowercase, no spaces. Agents see the channel name in their brief."
    ));
    assert_eq!(
        label_of(cx, window, "dialog-confirm").as_deref(),
        Some("Create channel")
    );
    type_text(cx, window, "general");
    press(cx, window, "enter");
    wait_until(cx, "the clash", |cx| {
        label_of(cx, window, "dialog-error").as_deref() == Some("#general already exists in demo.")
    });
    press(cx, window, "cmd-a backspace");
    type_text(cx, window, "Design Docs");
    press(cx, window, "enter");
    wait_until(cx, "the rule", |cx| {
        label_of(cx, window, "dialog-error").as_deref()
            == Some("Use lowercase letters, numbers and dashes.")
    });
    press(cx, window, "cmd-a backspace");
    type_text(cx, window, "design");
    press(cx, window, "tab");
    type_text(cx, window, "Mocks and reviews");
    click(cx, window, "dialog-confirm");
    wait_until(cx, "the new channel to open", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.nav
                .channel
                .and_then(|c| ws.state().channel(c))
                .is_some_and(|c| c.name == "design" && c.description == "Mocks and reviews")
                && ws.dialog.is_none()
        })
    });
    let design = shown_channel(cx, &ws).unwrap();
    wait_has(cx, window, format!("channel-{design}"));
    assert!(is_selected(cx, window, format!("channel-{design}")));
}
