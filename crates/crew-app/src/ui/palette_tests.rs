//! End to end: the ⌘K palette, with a real crewd on a temp home and a
//! fake claude, the window connected over the socket.

use std::path::Path;
use std::time::Duration;

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_proto::{Request, SearchResults, SearchTotals, ThreadHit, ThreadState};
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::diff_tests::{fake_editors, repo_with_readme, wait_for_launches};
use super::inspector_tests::run_of;
use super::palette::{DEBOUNCE, Remote};
use super::test_ui::{
    click, description_of, has, label_of, open, press, shows_within, type_text, wait_until,
};
use super::thread_tests::{agent, ask, go_to, world};
use super::workspace::Workspace;
use crate::recents::Recents;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

/// The palette's rows, as (label, description), top to bottom.
fn rows(cx: &mut TestAppContext, window: AnyWindowHandle) -> Vec<(String, String)> {
    (0..60usize)
        .map_while(|i| {
            let label = label_of(cx, window, ("palette-row", i))?;
            let sub = description_of(cx, window, ("palette-row", i)).unwrap_or_default();
            Some((label, sub))
        })
        .collect()
}

fn row_index(cx: &mut TestAppContext, window: AnyWindowHandle, label: &str, sub: &str) -> usize {
    rows(cx, window)
        .iter()
        .position(|(l, s)| l == label && s == sub)
        .unwrap_or_else(|| panic!("no row {label:?} {sub:?} in {:?}", rows(cx, window)))
}

fn shows_in_palette(cx: &mut TestAppContext, window: AnyWindowHandle, label: &str) -> bool {
    shows_within(cx, window, "palette", label)
}

/// Opens the palette with ⌘K and draws it, so its field has focus.
fn open_palette(cx: &mut TestAppContext, window: AnyWindowHandle) {
    press(cx, window, "cmd-k");
    wait_until(cx, "the palette", |cx| has(cx, window, "palette"));
}

/// Types `text`, waits out the debounce and then for `label` to show.
fn search(cx: &mut TestAppContext, window: AnyWindowHandle, text: &str, label: &str) {
    type_text(cx, window, text);
    cx.executor()
        .advance_clock(DEBOUNCE + Duration::from_millis(30));
    wait_until(cx, label, |cx| shows_in_palette(cx, window, label));
}

fn palette_open(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> bool {
    ws.read_with(cx, |ws, _| ws.palette().is_some())
}

fn wait_for_snapshot(cx: &mut TestAppContext, ws: &Entity<Workspace>, thread: ThreadId) {
    wait_until(cx, "the snapshot", |cx| {
        ws.read_with(cx, |ws, _| ws.state().thread(thread).is_some())
    });
}

#[gpui_kit::test]
fn cmd_k_opens_with_recent_and_actions(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open(cx, home.path());
    go_to(cx, window, &ws, w.channel, w.thread);
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.open_place(super::workspace::Place::Agent(w.agents[0]), window, cx)
        });
    })
    .unwrap();
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.page_agent()),
        Some(w.agents[0]),
        "its profile"
    );
    press(cx, window, "cmd-1");
    go_to(cx, window, &ws, w.channel, w.thread);

    open_palette(cx, window);
    assert!(shows_in_palette(cx, window, "Recent"));
    assert!(shows_in_palette(cx, window, "Actions"));
    assert_eq!(
        label_of(cx, window, "palette-scope").as_deref(),
        Some("demo")
    );
    assert_eq!(
        rows(cx, window),
        [
            ("fix the login".into(), "#general".into()),
            ("general".into(), "demo · 0 active".into()),
            ("Forge".into(), "Forge things · claude · haiku".into()),
            ("New thread in #general".into(), String::new()),
            ("Open Running now".into(), "0 running · 0 needs you".into()),
            ("Add project…".into(), String::new()),
        ],
        "no Stop all while nothing runs"
    );
    for hint in ["↑↓ move", "↵ open", "tab all projects", "esc close"] {
        assert!(shows_within(cx, window, "palette-footer", hint), "{hint}");
    }
    assert!(!shows_within(cx, window, "palette-footer", "⌘C copy path"));

    // ↑ from the first row wraps to the last, ↓ back.
    let selected = |cx: &mut TestAppContext| {
        ws.read_with(cx, |ws, cx| ws.palette().unwrap().read(cx).selected())
    };
    assert_eq!(selected(cx), 0);
    press(cx, window, "up");
    assert_eq!(selected(cx), 5);
    press(cx, window, "down");
    assert_eq!(selected(cx), 0);

    // Kept in the prefs file for next time.
    wait_until(cx, "the recents on disk", |_| {
        let saved = Recents::load(home.path());
        saved.threads.first() == Some(&w.thread)
            && saved.channel == Some(w.channel)
            && saved.agent == Some(w.agents[0])
    });
}

#[gpui_kit::test]
fn the_title_bar_search_opens_the_palette(cx: &mut TestAppContext) {
    let home = short_home();
    let (window, ws) = open(cx, home.path());
    click(cx, window, "title-search");
    wait_until(cx, "the palette", |cx| has(cx, window, "palette"));
    assert_eq!(
        label_of(cx, window, "palette-scope").as_deref(),
        Some("All projects"),
        "nothing open"
    );
    assert!(palette_open(cx, &ws));
}

#[gpui_kit::test]
fn typing_finds_a_thread_and_enter_opens_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let release: ThreadId = w.ask(Request::CreateThread {
        channel: w.channel,
        title: "plan the release".into(),
        first_line: false,
    });
    let (window, ws) = open(cx, home.path());
    wait_for_snapshot(cx, &ws, release);

    open_palette(cx, window);
    search(cx, window, "Release", "Threads · 1");
    assert_eq!(
        rows(cx, window),
        [
            ("plan the release".into(), "demo · #general".into()),
            ("New thread in #general: “Release”".into(), String::new()),
        ]
    );
    press(cx, window, "enter");
    wait_until(cx, "the thread to open", |cx| {
        ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(release))
    });
    assert!(!has(cx, window, "palette"));
    assert!(!palette_open(cx, &ws));

    // Channels and agents show as you type, before crewd answers.
    open_palette(cx, window);
    type_text(cx, window, "gen");
    assert!(shows_in_palette(cx, window, "Channels · 1"));
    let new_thread = row_index(cx, window, "New thread in #general: “gen”", "");
    for _ in 0..new_thread {
        press(cx, window, "down");
    }
    press(cx, window, "enter");
    ws.read_with(cx, |ws, cx| {
        assert!(ws.nav.drafting, "the new thread composer");
        assert_eq!(ws.nav.channel, Some(w.channel));
        assert_eq!(ws.composer.read(cx).text(cx), "gen");
    });
}

#[gpui_kit::test]
fn only_the_latest_search_lands(cx: &mut TestAppContext) {
    let home = short_home();
    let (window, ws) = open(cx, home.path());
    open_palette(cx, window);
    let palette = ws.read_with(cx, |ws, _| ws.palette().unwrap().clone());
    type_text(cx, window, "fix");
    let first = palette.read_with(cx, |p, _| p.seq());
    type_text(cx, window, "e");
    let latest = palette.read_with(cx, |p, _| p.seq());
    assert!(latest > first);
    let answer = |query: &str| Remote {
        query: query.into(),
        project: None,
        results: SearchResults {
            threads: vec![ThreadHit {
                thread: ThreadId::new(),
                title: "fix the login".into(),
                channel: ChannelId::new(),
                state: ThreadState::Idle,
            }],
            totals: SearchTotals {
                threads: 1,
                ..SearchTotals::default()
            },
            ..SearchResults::default()
        },
    };
    palette.update(cx, |p, cx| p.land(first, answer("fix"), cx));
    assert_eq!(palette.read_with(cx, |p, _| p.remote_query()), None);
    palette.update(cx, |p, cx| p.land(latest, answer("fixe"), cx));
    assert_eq!(
        palette.read_with(cx, |p, _| p.remote_query()).as_deref(),
        Some("fixe")
    );
}

#[gpui_kit::test]
fn tab_toggles_all_projects(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let other_root = tmp.path().join("other");
    std::fs::create_dir_all(&other_root).unwrap();
    let other: ProjectId = w.ask(Request::CreateProject {
        name: "other".into(),
        root: other_root.to_string_lossy().into_owned(),
        main_branch: "main".into(),
        work_mode: None,
        copy_files: vec![],
        setup_command: None,
    });
    let general: ChannelId = w.ask(Request::CreateChannel {
        project: other,
        name: "general".into(),
    });
    let docs: ThreadId = w.ask(Request::CreateThread {
        channel: general,
        title: "fix the docs".into(),
        first_line: false,
    });
    let (window, ws) = open(cx, home.path());
    wait_for_snapshot(cx, &ws, docs);
    go_to(cx, window, &ws, w.channel, w.thread);

    open_palette(cx, window);
    search(cx, window, "fix", "Threads · 1");
    assert_eq!(
        rows(cx, window)[0],
        ("fix the login".into(), "#general".into())
    );
    press(cx, window, "tab");
    assert_eq!(
        label_of(cx, window, "palette-scope").as_deref(),
        Some("All projects")
    );
    cx.executor()
        .advance_clock(DEBOUNCE + Duration::from_millis(30));
    wait_until(cx, "both projects' threads", |cx| {
        shows_in_palette(cx, window, "Threads · 2")
    });
    let found = rows(cx, window);
    assert!(
        found.contains(&("fix the docs".into(), "other · #general".into())),
        "{found:?}"
    );
    assert!(found.contains(&("fix the login".into(), "demo · #general".into())));
    press(cx, window, "tab");
    assert_eq!(
        label_of(cx, window, "palette-scope").as_deref(),
        Some("demo")
    );
}

/// A claude that edits README.md and adds a file, then ends.
fn editing_claude(dir: &Path) -> std::path::PathBuf {
    fake_claude(
        dir,
        &format!(
            "printf 'hello\\nthere\\n' > README.md\nprintf 'one\\n' > new.txt\ncat '{}'",
            fixture("edit_and_test.jsonl").display()
        ),
    )
}

#[gpui_kit::test]
fn a_changed_worktree_file_says_so_and_opens_its_review(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), editing_claude(tmp.path()));
    let w = world(home.path(), &root, &[]);
    let forge: AgentId = ask(
        &w.client,
        Request::CreateAgent {
            spec: crew_core::agent::AgentSpec {
                permission_mode: PermissionMode::EditFiles,
                ..agent("Forge", "Builds")
            },
        },
    );
    let (window, ws) = open(cx, home.path());
    w.post("@Forge fix it");
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);
    let (branch, worktree) = ws.read_with(cx, |ws, _| {
        let wt = ws
            .state()
            .thread(w.thread)
            .unwrap()
            .worktree
            .clone()
            .unwrap();
        (wt.branch, wt.path)
    });
    let launches = fake_editors(cx, window, &ws, &["Zed"], false);

    open_palette(cx, window);
    search(cx, window, "readme", "Files in worktrees · 2");
    // Nothing is open, so all projects are searched and named.
    let changed = row_index(
        cx,
        window,
        "README.md",
        &format!("demo · {branch} · changed"),
    );
    let main = row_index(cx, window, "README.md", "demo · main");

    // ⌘C on the changed row copies the worktree file's path.
    for _ in 0..changed {
        press(cx, window, "down");
    }
    assert!(shows_within(cx, window, "palette-footer", "⌘C copy path"));
    press(cx, window, "cmd-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text());
    assert_eq!(
        copied,
        Some(worktree.join("README.md").to_string_lossy().into_owned())
    );

    press(cx, window, "enter");
    wait_until(cx, "the review on README.md", |cx| {
        ws.read_with(cx, |ws, _| ws.review_file().as_deref() == Some("README.md"))
    });
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.open_thread_id()),
        Some(w.thread)
    );

    // The unchanged copy in the project opens in the editor.
    open_palette(cx, window);
    search(cx, window, "readme", "Files in worktrees · 2");
    for _ in 0..main {
        press(cx, window, "down");
    }
    press(cx, window, "enter");
    let root = root.to_string_lossy().into_owned();
    assert_eq!(
        wait_for_launches(cx, &launches, 1),
        [vec![
            "-a".to_string(),
            "Zed".into(),
            root.clone(),
            format!("{root}/README.md"),
        ]]
    );
}

#[gpui_kit::test]
fn cmd_shift_r_opens_running_now(cx: &mut TestAppContext) {
    let home = short_home();
    let (window, ws) = open(cx, home.path());
    press(cx, window, "cmd-shift-r");
    assert!(ws.read_with(cx, |ws, _| ws.running.is_some()));
    press(cx, window, "cmd-1");
    assert!(ws.read_with(cx, |ws, _| ws.running.is_none()));

    open_palette(cx, window);
    let running = row_index(cx, window, "Open Running now", "0 running · 0 needs you");
    click(cx, window, ("palette-row", running));
    assert!(ws.read_with(cx, |ws, _| ws.running.is_some()));
    assert!(!has(cx, window, "palette"));
}

#[gpui_kit::test]
fn esc_closes_the_palette_and_gives_focus_back(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    go_to(cx, window, &ws, w.channel, w.thread);
    let composer_focused = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| {
            ws.read(cx).composer.read(cx).focused(window, cx)
        })
        .unwrap()
    };
    assert!(composer_focused(cx));

    open_palette(cx, window);
    assert!(!composer_focused(cx));
    type_text(cx, window, "fix");
    press(cx, window, "escape");
    assert!(!has(cx, window, "palette"));
    assert!(!palette_open(cx, &ws));
    assert!(composer_focused(cx), "back where it was");
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.open_thread_id()),
        Some(w.thread)
    );
}

#[gpui_kit::test]
fn a_long_section_shows_five_then_all(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let mut last = w.channel;
    for n in 1..=5 {
        last = w.ask(Request::CreateChannel {
            project: w.project,
            name: format!("general-{n}"),
        });
    }
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the channels", |cx| {
        ws.read_with(cx, |ws, _| ws.state().channel(last).is_some())
    });
    open_palette(cx, window);
    type_text(cx, window, "general");
    assert!(shows_in_palette(cx, window, "Channels · 6"));
    let show_all = row_index(cx, window, "Show all 6", "");
    assert_eq!(show_all, 5, "after five channels");
    click(cx, window, ("palette-row", show_all));
    assert!(has(cx, window, "palette"), "stays open");
    let channels = rows(cx, window)
        .iter()
        .filter(|(_, sub)| sub == "demo")
        .count();
    assert_eq!(channels, 6);
    assert!(
        !rows(cx, window)
            .iter()
            .any(|(l, _)| l.starts_with("Show all"))
    );

    // The click left the keys working in the field.
    let focused = cx
        .update_window(window, |_, window, cx| {
            ws.read(cx)
                .palette()
                .unwrap()
                .read(cx)
                .field_focused(window, cx)
        })
        .unwrap();
    assert!(focused, "the cursor stays in the field");
    let count = rows(cx, window).len();
    let selected = |cx: &mut TestAppContext| {
        ws.read_with(cx, |ws, cx| ws.palette().unwrap().read(cx).selected())
    };
    press(cx, window, "down");
    let before = selected(cx);
    press(cx, window, "up");
    assert_eq!(selected(cx), (before + count - 1) % count);
    press(cx, window, "escape");
    assert!(!has(cx, window, "palette"));
}

#[gpui_kit::test]
fn a_typing_burst_sends_one_search(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    wait_for_snapshot(cx, &ws, w.thread);
    open_palette(cx, window);
    let palette = ws.read_with(cx, |ws, _| ws.palette().unwrap().clone());
    for key in ["z", "z", "z"] {
        type_text(cx, window, key);
        cx.executor().advance_clock(Duration::from_millis(50));
    }
    assert_eq!(palette.read_with(cx, |p, _| p.sent()), 0, "still typing");
    assert!(shows_in_palette(cx, window, "Searching…"));
    assert!(!shows_in_palette(cx, window, "No matches"));
    cx.executor().advance_clock(DEBOUNCE);
    wait_until(cx, "the answer", |cx| {
        palette.read_with(cx, |p, _| p.remote_query()).as_deref() == Some("zzz")
    });
    assert_eq!(palette.read_with(cx, |p, _| p.sent()), 1);
    assert!(!shows_in_palette(cx, window, "Searching…"));
    assert_eq!(
        rows(cx, window),
        [("New thread in #general: “zzz”".into(), String::new())],
        "only the action matches"
    );
}

#[gpui_kit::test]
fn a_failed_search_says_so_until_the_next_query(cx: &mut TestAppContext) {
    // No crewd: every search fails.
    let home = short_home();
    let (window, ws) = open(cx, home.path());
    open_palette(cx, window);
    let palette = ws.read_with(cx, |ws, _| ws.palette().unwrap().clone());
    search(cx, window, "fix", "Search isn't available right now.");
    assert!(!shows_in_palette(cx, window, "No matches"));
    assert!(!shows_in_palette(cx, window, "Searching…"));
    assert_eq!(palette.read_with(cx, |p, _| p.sent()), 1);
    type_text(cx, window, "e");
    assert!(!shows_in_palette(
        cx,
        window,
        "Search isn't available right now."
    ));
    assert!(shows_in_palette(cx, window, "Searching…"));
}

#[gpui_kit::test]
fn stop_all_shows_while_a_run_is_live(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
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
    let (window, ws) = open(cx, home.path());
    w.post("@Forge look");
    super::thread_tests::live_run(cx, &ws, w.thread);

    open_palette(cx, window);
    let stop = row_index(cx, window, "Stop all agents", "");
    click(cx, window, ("palette-row", stop));
    assert!(!has(cx, window, "palette"));
    wait_until(cx, "every run to stop", |cx| {
        ws.read_with(cx, |ws, _| ws.state().running().is_empty())
    });
    open_palette(cx, window);
    assert!(!rows(cx, window).iter().any(|(l, _)| l == "Stop all agents"));
}

/// Projects named `names`, each with `#general`, rooted under `dir`, added
/// through the world's client. Returns their ids and #general ids.
fn more_projects(
    w: &super::thread_tests::World,
    dir: &Path,
    names: &[&str],
) -> Vec<(ProjectId, ChannelId)> {
    names
        .iter()
        .map(|name| {
            let root = dir.join(name);
            repo_with_readme(&root);
            let project: ProjectId = w.ask(Request::CreateProject {
                name: (*name).into(),
                root: root.to_string_lossy().into_owned(),
                main_branch: "main".into(),
                work_mode: None,
                copy_files: vec![],
                setup_command: None,
            });
            let channel: ChannelId = w.ask(Request::CreateChannel {
                project,
                name: "general".into(),
            });
            (project, channel)
        })
        .collect()
}

#[gpui_kit::test]
fn typing_finds_projects_first_and_enter_opens_one(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let w = world(home.path(), &root, &[]);
    let added = more_projects(&w, tmp.path(), &["supapick", "supameter"]);
    let (pick, _) = added[0];
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the projects", |cx| {
        ws.read_with(cx, |ws, _| ws.state().channel(added[1].1).is_some())
    });
    let path = |name: &str| super::title_bar::home_relative(&tmp.path().join(name));

    open_palette(cx, window);
    type_text(cx, window, "supa");
    assert!(shows_in_palette(cx, window, "Projects · 2"));
    let found = rows(cx, window);
    assert_eq!(
        found[..2],
        [
            ("supameter".to_string(), path("supameter")),
            ("supapick".to_string(), path("supapick")),
        ],
        "first, by name"
    );
    let pick_row = row_index(cx, window, "supapick", &path("supapick"));
    for _ in 0..pick_row {
        press(cx, window, "down");
    }
    press(cx, window, "enter");
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.page_project(), Some(pick), "its overview");
        assert_eq!(ws.recents.project, Some(pick));
    });

    // Recent lists it next time.
    open_palette(cx, window);
    assert!(
        rows(cx, window).contains(&("supapick".into(), path("supapick"))),
        "{:?}",
        rows(cx, window)
    );
}

#[gpui_kit::test]
fn channels_match_with_their_project_name(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let w = world(home.path(), &root, &[]);
    let added = more_projects(&w, tmp.path(), &["supapick", "supameter"]);
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the projects", |cx| {
        ws.read_with(cx, |ws, _| ws.state().channel(added[1].1).is_some())
    });

    open_palette(cx, window);
    type_text(cx, window, "supapick gen");
    assert!(shows_in_palette(cx, window, "Channels · 1"));
    let row = row_index(cx, window, "general", "supapick");
    for _ in 0..row {
        press(cx, window, "down");
    }
    press(cx, window, "enter");
    assert_eq!(ws.read_with(cx, |ws, _| ws.nav.channel), Some(added[0].1));

    // Now in supapick: its own channels only, until tab.
    open_palette(cx, window);
    type_text(cx, window, "supa gen");
    assert!(shows_in_palette(cx, window, "Channels · 1"));
    press(cx, window, "tab");
    assert!(shows_in_palette(cx, window, "Channels · 2"));
    let found = rows(cx, window);
    assert!(found.contains(&("general".into(), "supameter".into())));
    assert!(found.contains(&("general".into(), "supapick".into())));
}

#[gpui_kit::test]
fn files_name_their_project_only_across_projects(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let w = world(home.path(), &root, &[]);
    let added = more_projects(&w, tmp.path(), &["supapick"]);
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the projects", |cx| {
        ws.read_with(cx, |ws, _| ws.state().channel(added[0].1).is_some())
    });

    open_palette(cx, window);
    search(cx, window, "readme", "Files in worktrees · 2");
    let found = rows(cx, window);
    assert!(found.contains(&("README.md".into(), "demo · main".into())));
    assert!(found.contains(&("README.md".into(), "supapick · main".into())));
    press(cx, window, "escape");

    go_to(cx, window, &ws, w.channel, w.thread);
    open_palette(cx, window);
    search(cx, window, "readme", "Files in worktrees · 1");
    assert!(rows(cx, window).contains(&("README.md".into(), "main".into())));
}

#[gpui_kit::test]
fn name_hits_come_before_files(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    for n in 0..7 {
        std::fs::write(root.join(format!("supa-{n}.txt")), "x").unwrap();
    }
    let w = world(home.path(), &root, &[]);
    let added = more_projects(&w, tmp.path(), &["supapick"]);
    let _: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "supa-talk".into(),
    });
    let _: ThreadId = w.ask(Request::CreateThread {
        channel: w.channel,
        title: "supa launch".into(),
        first_line: false,
    });
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the projects", |cx| {
        ws.read_with(cx, |ws, _| ws.state().channel(added[0].1).is_some())
    });

    open_palette(cx, window);
    search(cx, window, "supa", "Files in worktrees · 7");
    let found = rows(cx, window);
    let position = |label: &str| found.iter().position(|(l, _)| l == label).unwrap();
    let first_file = found.iter().position(|(l, _)| l.ends_with(".txt")).unwrap();
    assert!(position("supapick") < first_file);
    assert!(position("supa launch") < first_file);
    assert!(position("supa-talk") < first_file);
    let files = found.iter().filter(|(l, _)| l.ends_with(".txt")).count();
    assert_eq!(files, 5, "capped");
    assert!(found.iter().any(|(l, _)| l == "Show all 7"));
}
