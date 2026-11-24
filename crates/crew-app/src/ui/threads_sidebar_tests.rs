//! End to end: the sidebar's Threads form and the switch between the two
//! forms, with a real crewd on a temp home and a fake claude.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, Focusable as _, TestAppContext};

use super::diff_tests::repo_with_readme;
use super::inspector_tests::run_of;
use super::settings::Page;
use super::sidebar::row_probe::row_of;
use super::test_ui::{bounds_of, click, has, label_of, open, press, type_text, wait_until};
use super::thread_tests::{World, agent, open_thread, world};
use super::workspace::Workspace;
use crate::client::ClientHandle;
use crate::prefs::{Appearance, SidebarMode};
use crate::test_support::{Daemon, fake_claude, recording_proxy, short_home};
use crate::theme::scale;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A claude whose prompt ending in `WAIT` waits for the file `go`, and
/// ending in `EDIT` changes `README.md` and adds `new.txt` where it
/// runs. Then it replies "ok".
fn claude(dir: &Path) -> PathBuf {
    let at = dir.display();
    let run = [
        "for last in \"$@\"; do :; done".to_string(),
        format!("case \"$last\" in *WAIT) while [ ! -e '{at}/go' ]; do sleep 0.02; done;; esac"),
        "case \"$last\" in *EDIT) printf 'hello\\nthere\\n' > README.md; printf 'one\\ntwo\\n' > new.txt;; esac".to_string(),
        r#"printf '{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":[]}\n'"#.to_string(),
        r#"printf '{"type":"result","subtype":"success","is_error":false,"result":"ok","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}\n'"#.to_string(),
    ]
    .join("\n");
    fake_claude(dir, &run)
}

fn thread(w: &World, channel: ChannelId, title: &str) -> ThreadId {
    w.ask(Request::CreateThread {
        channel,
        title: title.into(),
    })
}

fn post(w: &World, thread: ThreadId, body: &str) {
    let _: MessageId = w.ask(Request::PostMessage {
        thread,
        body: body.into(),
    });
}

/// Asks to use Bash in `run`, as crew-mcp would, without waiting for the
/// answer.
fn ask_in_background(client: &ClientHandle, run: RunId) {
    let client = client.clone();
    std::thread::spawn(move || {
        let _ = futures::executor::block_on(client.request(Request::PermissionAsk {
            run,
            tool: "Bash".into(),
            input: serde_json::json!({ "command": "cargo update" }),
            tool_use_id: None,
        }));
    });
}

fn row(thread: ThreadId) -> String {
    format!("thread-row-{thread}")
}

/// Waits for `id` to read `want`, letting timers run meanwhile.
fn wait_label(cx: &mut TestAppContext, window: AnyWindowHandle, id: String, want: &str) {
    let mut last = None;
    wait_until(
        cx,
        &format!("{id} to read {want:?} (last {last:?})"),
        |cx| {
            cx.executor().advance_clock(Duration::from_millis(600));
            last = label_of(cx, window, id.clone());
            last.as_deref() == Some(want)
        },
    );
}

fn wait_has(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) {
    wait_until(cx, &id.clone(), |cx| {
        cx.executor().advance_clock(Duration::from_millis(600));
        has(cx, window, id.clone())
    });
}

fn is_selected(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) -> bool {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).and_then(|e| e.selected()) == Some(true)
    })
    .unwrap()
}

fn hover(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn mode(cx: &mut TestAppContext) -> SidebarMode {
    cx.update(|cx| scale::appearance(cx).sidebar_mode)
}

fn to_threads(cx: &mut TestAppContext, window: AnyWindowHandle) {
    assert_eq!(
        label_of(cx, window, "sidebar-switch").as_deref(),
        Some("Show threads")
    );
    click(cx, window, "sidebar-switch");
    wait_until(cx, "the threads sidebar", |cx| {
        has(cx, window, "threads-search")
    });
}

fn set_now(cx: &mut TestAppContext, ws: &Entity<Workspace>, now: i64) {
    ws.update(cx, |ws, cx| {
        ws.sidebar.threads.now = Some(now);
        cx.notify();
    });
    cx.run_until_parked();
}

fn focus_search(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.sidebar
                .threads
                .search
                .update(cx, |input, cx| input.focus(window, cx));
        });
    })
    .unwrap();
    cx.run_until_parked();
}

fn list_focused(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>) -> bool {
    cx.update_window(window, |_, window, cx| {
        ws.read(cx).sidebar.threads.list_focus.is_focused(window)
    })
    .unwrap()
}

fn search_focused(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
) -> bool {
    cx.update_window(window, |_, window, cx| {
        ws.read(cx)
            .sidebar
            .threads
            .search
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
    })
    .unwrap()
}

#[gpui_kit::test]
fn the_switch_and_appearance_change_the_sidebar_and_keep_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the project", |cx| has(cx, window, row_of(w.channel)));

    // The switch's tip sits under it, as on the board.
    hover(cx, window, "sidebar-switch".into());
    assert_eq!(
        label_of(cx, window, "sidebar-switch-tip").as_deref(),
        Some("Show threads · switch sidebar")
    );
    let tip = bounds_of(cx, window, "sidebar-switch-tip").unwrap();
    let button = bounds_of(cx, window, "sidebar-switch").unwrap();
    assert!(tip.origin.y >= button.origin.y + button.size.height);
    to_threads(cx, window);
    assert_eq!(mode(cx), SidebarMode::Threads);
    hover(cx, window, "sidebar-switch".into());
    assert_eq!(
        label_of(cx, window, "sidebar-switch-tip").as_deref(),
        Some("Show projects · switch sidebar")
    );
    assert!(!has(cx, window, "sidebar-filter"), "the projects are gone");
    assert_eq!(
        label_of(cx, window, "sidebar-switch").as_deref(),
        Some("Show projects")
    );
    assert_eq!(
        label_of(cx, window, "threads-switcher").as_deref(),
        Some("All projects")
    );
    wait_has(cx, window, row(w.thread));
    wait_until(cx, "the choice saved", |_| {
        Appearance::load(home.path()).sidebar_mode == SidebarMode::Threads
    });

    // Settings › Appearance says so, and switches back.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_settings(Page::Appearance, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        label_of(cx, window, "appearance-sidebar-threads").as_deref(),
        Some("Threads (selected)")
    );
    click(cx, window, "appearance-sidebar-projects");
    assert_eq!(mode(cx), SidebarMode::Projects);
    wait_until(cx, "the choice saved", |_| {
        Appearance::load(home.path()).sidebar_mode == SidebarMode::Projects
    });
    click(cx, window, "appearance-sidebar-threads");
    assert_eq!(mode(cx), SidebarMode::Threads);
    press(cx, window, "escape");
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.leave_pages(cx));
        window.render_frame(cx);
    })
    .unwrap();
    assert!(has(cx, window, "threads-search"));

    // The switch goes back to projects.
    click(cx, window, "sidebar-switch");
    assert!(has(cx, window, "sidebar-filter"));
    assert_eq!(mode(cx), SidebarMode::Projects);
}

/// demo with Forge (edits) and Scout (reads), and four threads: one
/// changed by Forge, one Scout answered, one Scout works on and one
/// where Scout waits on you.
struct Busy {
    _tmp: tempfile::TempDir,
    _home: tempfile::TempDir,
    _daemon: Daemon,
    w: World,
    answered: ThreadId,
    working: ThreadId,
    needs: ThreadId,
    window: AnyWindowHandle,
    ws: Entity<Workspace>,
}

fn busy(cx: &mut TestAppContext) -> Busy {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let home = short_home();
    let daemon = Daemon::start_with(home.path().to_path_buf(), claude(tmp.path()));
    let w = world(home.path(), &root, &["Scout"]);
    let scout = w.agents[0];
    let forge: AgentId = w.ask(Request::CreateAgent {
        spec: crew_core::agent::AgentSpec {
            permission_mode: PermissionMode::EditFiles,
            ..agent("Forge", "Builds")
        },
    });
    let answered = thread(&w, w.channel, "how does it work");
    let working = thread(&w, w.channel, "write the docs");
    let needs = thread(&w, w.channel, "bump the deps");
    let (window, ws) = open(cx, home.path());
    post(&w, w.thread, "@Forge EDIT");
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);
    post(&w, answered, "@Scout explain");
    run_of(cx, &ws, answered, scout, RunStatus::Done);
    post(&w, working, "@Scout WAIT");
    run_of(cx, &ws, working, scout, RunStatus::Working);
    post(&w, needs, "@Scout WAIT");
    let run = run_of(cx, &ws, needs, scout, RunStatus::Working).id;
    ask_in_background(&w.client, run);
    wait_until(cx, "the ask", |cx| {
        ws.read_with(cx, |ws, _| !ws.state().pending_approvals(needs).is_empty())
    });
    to_threads(cx, window);
    Busy {
        _tmp: tmp,
        _home: home,
        _daemon: daemon,
        w,
        answered,
        working,
        needs,
        window,
        ws,
    }
}

#[gpui_kit::test]
fn threads_group_into_needs_you_working_recent_and_settled(cx: &mut TestAppContext) {
    let b = busy(cx);
    let (window, ws) = (b.window, b.ws.clone());
    let edited = b.w.thread;
    wait_label(cx, window, "threads-group-needs".into(), "Needs you · 1");
    wait_label(cx, window, "threads-group-working".into(), "Working · 1");
    wait_label(cx, window, "threads-group-recent".into(), "Recent · 2");
    assert!(!has(cx, window, "threads-settled"));

    // Each row: where it is, its status, its title, then its branch and
    // changes, or "answer only".
    assert_eq!(
        label_of(cx, window, row(b.needs)).as_deref(),
        Some("bump the deps")
    );
    assert_eq!(
        label_of(cx, window, format!("thread-row-place-{}", b.needs)).as_deref(),
        Some("demo · #general")
    );
    let status = |cx: &mut TestAppContext, t: ThreadId| {
        label_of(cx, window, format!("thread-row-status-{t}"))
    };
    assert_eq!(status(cx, b.needs).as_deref(), Some("Needs you"));
    assert_eq!(status(cx, b.working).as_deref(), Some("Working"));
    assert_eq!(status(cx, b.answered).as_deref(), Some("Done"));
    assert_eq!(status(cx, edited).as_deref(), Some("Done"));
    assert!(has(cx, window, format!("thread-row-answer-{}", b.answered)));
    let branch = ws.read_with(cx, |ws, _| {
        ws.state()
            .thread(edited)
            .and_then(|t| t.worktree.as_ref())
            .map(|w| w.branch.clone())
            .unwrap()
    });
    assert_eq!(
        label_of(cx, window, format!("thread-row-branch-{edited}")),
        Some(branch.clone())
    );
    wait_label(cx, window, format!("thread-row-diff-{edited}"), "+3 −1");
    assert!(!has(cx, window, format!("thread-row-answer-{edited}")));

    // The popover sums the thread up and opens it or its changes.
    hover(cx, window, row(edited));
    wait_has(cx, window, "thread-popover".into());
    // Level with the group heading above the row, as on the board.
    let pop_top = f32::from(bounds_of(cx, window, "thread-popover").unwrap().origin.y);
    let row_top = f32::from(bounds_of(cx, window, row(edited)).unwrap().origin.y);
    assert!(
        (row_top - pop_top - 27.).abs() < 1.,
        "{row_top} vs {pop_top}"
    );
    let pop =
        |cx: &mut TestAppContext, key: &str| label_of(cx, window, format!("thread-popover-{key}"));
    assert_eq!(pop(cx, "status").as_deref(), Some("Done"));
    assert_eq!(pop(cx, "project").as_deref(), Some("demo · #general"));
    assert_eq!(pop(cx, "branch"), Some(branch));
    assert_eq!(
        pop(cx, "changes").as_deref(),
        Some("2 files · +3 −1 · uncommitted")
    );
    assert_eq!(pop(cx, "agents").as_deref(), Some("Forge claude · haiku"));
    assert!(
        pop(cx, "activity")
            .unwrap()
            .starts_with("2 messages · 1 run · "),
    );
    assert_eq!(pop(cx, "last").as_deref(), Some("now"));
    assert!(has(cx, window, "thread-popover-open"));
    click(cx, window, "thread-popover-review");
    wait_until(cx, "the review", |cx| {
        ws.read_with(cx, |ws, _| ws.review.is_some())
    });
    ws.update(cx, |ws, cx| ws.leave_pages(cx));
    cx.run_until_parked();

    hover(cx, window, row(b.needs));
    wait_label(
        cx,
        window,
        "thread-popover-status".into(),
        "Needs you · Scout wants to run a command",
    );
    click(cx, window, "thread-popover-open");
    assert_eq!(ws.read_with(cx, |ws, _| ws.nav.thread), Some(b.needs));
    assert!(is_selected(cx, window, row(b.needs)));

    // A week later the finished threads settle; the live ones stay.
    set_now(cx, &ws, crate::ui::common::now_ms() + 8 * DAY_MS);
    wait_label(cx, window, "threads-settled".into(), "Settled · 2");
    assert!(!has(cx, window, "threads-group-recent"));
    assert!(!has(cx, window, row(edited)), "Settled starts closed");
    assert!(has(cx, window, row(b.needs)));
    click(cx, window, "threads-settled");
    assert!(has(cx, window, row(edited)));
    assert!(has(cx, window, row(b.answered)));
    click(cx, window, "threads-settled");
    assert!(!has(cx, window, row(edited)));

    // Searching opens Settled; it still closes.
    focus_search(cx, window, &ws);
    type_text(cx, window, "login");
    assert!(has(cx, window, row(edited)));
    click(cx, window, "threads-settled");
    assert!(!has(cx, window, row(edited)));
    click(cx, window, "threads-settled");
    assert!(has(cx, window, row(edited)));
    focus_search(cx, window, &ws);
    press(cx, window, "escape");
    assert!(!has(cx, window, row(edited)), "closed as you left it");
    click(cx, window, "threads-settled");
    assert!(has(cx, window, row(b.answered)));

    // Archived threads leave the list.
    let _: serde_json::Value = b.w.ask(Request::ArchiveThread {
        thread: b.answered,
        archived: true,
    });
    wait_label(cx, window, "threads-settled".into(), "Settled · 1");
    assert!(!has(cx, window, row(b.answered)));
}

#[gpui_kit::test]
fn the_project_switcher_picks_a_project_and_its_channels(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let make = |name: &str| -> ProjectId {
        let root = tmp.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        w.ask(Request::CreateProject {
            name: name.into(),
            root: root.to_string_lossy().into_owned(),
            main_branch: "main".into(),
            work_mode: None,
            copy_files: vec![],
            setup_command: None,
        })
    };
    let shop = make("shop");
    let quiet = make("quiet");
    let channel = |project: ProjectId, name: &str| -> ChannelId {
        w.ask(Request::CreateChannel {
            project,
            name: name.into(),
        })
    };
    let general = channel(shop, "general");
    let bugs = channel(shop, "bugs");
    channel(quiet, "general");
    let cart = thread(&w, general, "add the cart");
    let crash = thread(&w, bugs, "fix the crash");
    let _: serde_json::Value = w.ask(Request::PinProject {
        project: shop,
        pinned: true,
    });
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the projects", |cx| has(cx, window, row_of(w.channel)));
    to_threads(cx, window);
    wait_has(cx, window, row(cart));
    assert!(has(cx, window, row(w.thread)));
    assert!(!has(cx, window, "threads-chips"));

    // The count follows Show, as the list does.
    click(cx, window, "sidebar-filter-options");
    click(cx, window, "sidebar-show-pinned");
    press(cx, window, "escape");
    assert!(!has(cx, window, row(w.thread)));
    click(cx, window, "threads-switcher");
    assert_eq!(
        label_of(cx, window, "threads-switcher-count").as_deref(),
        Some("2 threads")
    );
    press(cx, window, "escape");
    click(cx, window, "sidebar-filter-options");
    click(cx, window, "sidebar-show-all");
    press(cx, window, "escape");

    click(cx, window, "threads-switcher");
    assert!(has(cx, window, "threads-switcher-menu"));
    assert_eq!(
        label_of(cx, window, "threads-switcher").as_deref(),
        Some("All projects")
    );
    assert_eq!(
        label_of(cx, window, "threads-switcher-count").as_deref(),
        Some("3 threads")
    );
    assert!(is_selected(cx, window, "switcher-all".into()));
    assert!(has(cx, window, format!("switcher-project-{shop}")));
    assert!(has(cx, window, format!("switcher-project-{}", w.project)));
    // quiet has no threads: it waits under "1 more".
    assert!(!has(cx, window, format!("switcher-project-{quiet}")));
    assert_eq!(
        label_of(cx, window, "switcher-more").as_deref(),
        Some("1 more")
    );
    click(cx, window, "switcher-more");
    assert!(has(cx, window, format!("switcher-project-{quiet}")));
    assert!(has(cx, window, "switcher-add"));

    // Finding narrows the projects; the keys pick one.
    type_text(cx, window, "sho");
    assert!(!has(cx, window, format!("switcher-project-{}", w.project)));
    press(cx, window, "down enter");
    assert!(!has(cx, window, "threads-switcher-menu"));
    assert_eq!(
        label_of(cx, window, "threads-switcher").as_deref(),
        Some("shop")
    );
    assert!(
        search_focused(cx, window, &ws),
        "typing goes back to the search"
    );
    assert!(has(cx, window, row(cart)));
    assert!(has(cx, window, row(crash)));
    assert!(!has(cx, window, row(w.thread)), "demo's threads go");

    // Its channels show as chips; one narrows the list to it.
    assert_eq!(
        label_of(cx, window, format!("threads-chip-{general}")).as_deref(),
        Some("#general")
    );
    click(cx, window, format!("threads-chip-{bugs}"));
    assert!(is_selected(cx, window, format!("threads-chip-{bugs}")));
    assert!(has(cx, window, row(crash)));
    assert!(!has(cx, window, row(cart)));
    click(cx, window, format!("threads-chip-{bugs}"));
    assert!(has(cx, window, row(cart)));

    // Esc closes the switcher; All projects brings every thread back.
    click(cx, window, "threads-switcher");
    press(cx, window, "escape");
    assert!(!has(cx, window, "threads-switcher-menu"));
    assert!(search_focused(cx, window, &ws));
    click(cx, window, "threads-switcher");
    click(cx, window, "switcher-all");
    assert!(has(cx, window, row(w.thread)));
    assert!(!has(cx, window, "threads-chips"));

    // Add project… opens Add project.
    click(cx, window, "threads-switcher");
    click(cx, window, "switcher-add");
    assert!(ws.read_with(cx, |ws, _| ws.dialog.is_some()));
}

#[gpui_kit::test]
fn keys_move_through_the_threads_and_the_selection_survives_the_switch(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let docs = thread(&w, w.channel, "write the docs");
    let deps = thread(&w, w.channel, "bump the deps");
    let (window, ws) = open_thread(cx, home.path(), &w);

    // The open thread is selected in Threads, and its channel in
    // Projects after switching back.
    to_threads(cx, window);
    wait_has(cx, window, row(w.thread));
    assert!(is_selected(cx, window, row(w.thread)));
    assert!(!is_selected(cx, window, row(docs)));
    click(cx, window, "sidebar-switch");
    wait_until(cx, "the projects", |cx| has(cx, window, row_of(w.channel)));
    assert!(is_selected(cx, window, row_of(w.channel)));
    click(cx, window, "sidebar-switch");
    wait_has(cx, window, row(docs));

    // ↓ from the search moves into the list, from the open thread; there
    // ↑/↓ move and ↵ opens, keeping the keys in the list.
    focus_search(cx, window, &ws);
    let order = ws.read_with(cx, |ws, _| ws.sidebar.threads.order.clone());
    assert_eq!(order.len(), 3);
    let at = order.iter().position(|t| *t == w.thread).unwrap();
    let (there, back, to) = if at > 0 {
        ("up", "down", at - 1)
    } else {
        ("down", "up", at + 1)
    };
    press(cx, window, there);
    assert!(list_focused(cx, window, &ws));
    assert!(is_selected(cx, window, row(order[to])));
    press(cx, window, back);
    assert!(!is_selected(cx, window, row(order[to])));
    press(cx, window, there);
    assert!(is_selected(cx, window, row(order[to])));
    press(cx, window, "enter");
    assert_eq!(ws.read_with(cx, |ws, _| ws.nav.thread), Some(order[to]));
    assert!(list_focused(cx, window, &ws));
    // Esc goes back to the search.
    press(cx, window, "escape");
    assert!(search_focused(cx, window, &ws));

    // Typing finds threads by title; the first is highlighted and ↵ in
    // the search opens it.
    type_text(cx, window, "DEPS");
    assert!(has(cx, window, row(deps)));
    assert!(!has(cx, window, row(docs)));
    assert!(is_selected(cx, window, row(deps)));
    press(cx, window, "enter");
    assert_eq!(ws.read_with(cx, |ws, _| ws.nav.thread), Some(deps));
    type_text(cx, window, "zzz");
    assert_eq!(
        label_of(cx, window, "threads-empty").as_deref(),
        Some("No threads match.")
    );

    // Esc clears the search, then leaves it.
    press(cx, window, "escape");
    assert!(has(cx, window, row(docs)));
    assert!(search_focused(cx, window, &ws));
    press(cx, window, "escape");
    assert!(!search_focused(cx, window, &ws));

    // The footer opens Running now and settings.
    click(cx, window, "threads-footer-running");
    assert!(ws.read_with(cx, |ws, _| ws.running.is_some()));
}

#[gpui_kit::test]
fn changes_ask_again_for_their_channels_only_once_they_pause(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let crewd_home = short_home();
    let _daemon = Daemon::start(crewd_home.path().to_path_buf());
    let home = short_home();
    let seen = recording_proxy(
        &home.path().join("crewd.sock"),
        crewd_home.path().join("crewd.sock"),
    );
    let w = world(crewd_home.path(), &tmp.path().join("repo"), &[]);
    let bugs: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "bugs".into(),
    });
    let crash = thread(&w, bugs, "fix the crash");
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the project", |cx| has(cx, window, row_of(w.channel)));
    to_threads(cx, window);
    wait_has(cx, window, row(crash));
    let asked = || -> Vec<Option<Vec<ChannelId>>> {
        seen.lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                Request::ThreadSummaries { channels, .. } => Some(channels.clone()),
                _ => None,
            })
            .collect()
    };
    wait_until(cx, "the first summaries", |_| !asked().is_empty());
    assert_eq!(asked(), [None], "everything, once");

    // Each change pushes the wait back.
    let rename = |cx: &mut TestAppContext, title: &str| {
        let _: serde_json::Value = w.ask(Request::RenameThread {
            thread: w.thread,
            title: title.into(),
        });
        let title = title.to_string();
        wait_until(cx, "the new title", |cx| {
            ws.read_with(cx, |ws, _| {
                ws.state().thread(w.thread).map(|t| t.title.clone()) == Some(title.clone())
            })
        });
    };
    rename(cx, "one");
    cx.executor().advance_clock(Duration::from_millis(300));
    rename(cx, "two");
    cx.executor().advance_clock(Duration::from_millis(300));
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(asked().len(), 1, "still settling");
    cx.executor().advance_clock(Duration::from_millis(300));
    wait_until(cx, "the changed channel", |_| asked().len() == 2);
    assert_eq!(
        asked()[1],
        Some(vec![w.channel]),
        "only the changed channel"
    );

    // Changes that never pause still go out within 2 s.
    for n in 0..6 {
        rename(cx, &format!("again {n}"));
        cx.executor().advance_clock(Duration::from_millis(400));
    }
    wait_until(cx, "a capped wait", |_| asked().len() >= 3);
}
