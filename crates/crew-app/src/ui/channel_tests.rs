//! End to end: the channel view, with a real crewd on a temp home and a
//! fake claude, the window connected over the socket.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, MessageId, RunId, ThreadId};
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::diff_tests::repo_with_readme;
use super::inspector_tests::run_of;
use super::test_ui::{
    bounds_of, click, has, label_of, open, open_tall, press, type_text, wait_to_show, wait_until,
};
use super::thread_tests::{World, agent, ask, world};
use super::workspace::Workspace;
use crate::client::{self, ClientHandle};
use crate::connection::ConnectionConfig;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

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

fn wait_label(cx: &mut TestAppContext, window: AnyWindowHandle, id: String, want: &str) {
    let mut last = None;
    wait_until(cx, &format!("{id} to read {want:?}"), |cx| {
        last = label_of(cx, window, id.clone());
        last.as_deref() == Some(want)
    });
}

/// Clicks the channel in the sidebar and waits for its view.
fn go_to_channel(cx: &mut TestAppContext, window: AnyWindowHandle, channel: ChannelId) {
    let row = format!("channel-{channel}");
    wait_until(cx, "the channel in the sidebar", |cx| {
        has(cx, window, row.clone())
    });
    click(cx, window, row);
    wait_until(cx, "the channel view", |cx| has(cx, window, "channel-view"));
}

fn hover(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn card(thread: ThreadId) -> String {
    format!("thread-{thread}")
}

fn top(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) -> f32 {
    bounds_of(cx, window, id.clone())
        .unwrap_or_else(|| panic!("{id} isn't shown"))
        .origin
        .y
        .as_f32()
}

fn left(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) -> f32 {
    bounds_of(cx, window, id.clone())
        .unwrap_or_else(|| panic!("{id} isn't shown"))
        .origin
        .x
        .as_f32()
}

#[gpui_kit::test]
fn cards_show_title_status_counts_and_branch(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let claude = fake_claude(
        tmp.path(),
        &format!(
            "printf 'hello\\nthere\\n' > README.md\nprintf 'one\\ntwo\\n' > new.txt\ncat '{}'",
            fixture("edit_and_test.jsonl").display()
        ),
    );
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), claude);
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
    let quiet: ThreadId = w.ask(Request::CreateThread {
        channel: w.channel,
        title: "plan the release".into(),
    });
    let (window, ws) = open(cx, home.path());
    w.post("@Forge fix it");
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);
    go_to_channel(cx, window, w.channel);

    let t = w.thread;
    assert_eq!(
        label_of(cx, window, "channel-title").as_deref(),
        Some("#general")
    );
    assert_eq!(
        label_of(cx, window, "channel-project").as_deref(),
        Some("in demo")
    );
    assert!(has(cx, window, "channel-add-description"));
    assert!(has(cx, window, "channel-settings"));
    assert_eq!(
        label_of(cx, window, format!("thread-title-{t}")).as_deref(),
        Some("fix the login")
    );
    assert_eq!(
        label_of(cx, window, format!("thread-status-{t}")).as_deref(),
        Some("Done")
    );
    let counts = format!("thread-counts-{t}");
    wait_until(cx, "the thread's counts", |cx| {
        label_of(cx, window, counts.clone()).is_some_and(|l| {
            l.starts_with("2 messages · 1 run · 0 handoffs · 2 files +3 −1 · ")
                && l.ends_with(" agent time")
        })
    });
    let branch = ws.read_with(cx, |ws, _| {
        ws.state()
            .thread(t)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.branch.clone())
            .unwrap()
    });
    assert!(branch.starts_with("crew/"));
    assert_eq!(
        label_of(cx, window, format!("thread-branch-{t}")),
        Some(branch)
    );
    assert_eq!(
        label_of(cx, window, format!("thread-worktree-{t}")).as_deref(),
        Some("2 uncommitted")
    );
    assert_eq!(
        label_of(cx, window, format!("thread-excerpt-{t}")).as_deref(),
        Some("Fixed.")
    );
    assert_eq!(
        label_of(cx, window, format!("thread-time-{t}")).as_deref(),
        Some("just now")
    );

    // A thread nobody worked in yet.
    assert_eq!(
        label_of(cx, window, format!("thread-status-{quiet}")).as_deref(),
        Some("Idle")
    );
    assert_eq!(
        label_of(cx, window, format!("thread-worktree-{quiet}")).as_deref(),
        Some("No worktree changes")
    );
    assert!(!has(cx, window, format!("thread-branch-{quiet}")));
    assert_eq!(
        label_of(cx, window, format!("thread-counts-{quiet}")).as_deref(),
        Some("0 messages · 0 runs · 0 handoffs · 0 files · 0s agent time")
    );

    // The stats sum up the channel.
    assert_eq!(
        label_of(cx, window, "channel-stat-threads").as_deref(),
        Some("Threads 2 · 2 this week")
    );
    assert_eq!(
        label_of(cx, window, "channel-stat-files").as_deref(),
        Some("Files changed this week 2 +3 −1")
    );
    assert_eq!(
        label_of(cx, window, "channel-stat-needs").as_deref(),
        Some("Needs you 0")
    );

    // Hovering shows the actions; Review changes only with changes.
    hover(cx, window, card(t));
    assert!(has(cx, window, format!("thread-open-{t}")));
    assert!(has(cx, window, format!("thread-review-{t}")));
    assert!(
        !has(cx, window, format!("thread-stop-{t}")),
        "nothing works"
    );
    assert!(!has(cx, window, format!("thread-time-{t}")));
    click(cx, window, format!("thread-more-{t}"));
    assert!(has(cx, window, "popup-menu"), "the overflow menu");
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.open_thread_id()),
        None,
        "the menu button doesn't open the thread"
    );
    press(cx, window, "escape");
    hover(cx, window, card(t));
    click(cx, window, format!("thread-open-{t}"));
    wait_until(cx, "the thread to open", |cx| {
        ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(t))
    });

    // Back in the channel, the thread you had open is the selected card.
    go_to_channel(cx, window, w.channel);
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.channel_view.selected, Some(t));
    });
}

/// A project with four threads: one waiting on you, one working, one
/// quiet and one last touched a month ago.
struct Busy {
    _tmp: tempfile::TempDir,
    _home: tempfile::TempDir,
    _daemon: Daemon,
    client: ClientHandle,
    _events: futures::channel::mpsc::UnboundedReceiver<client::ClientEvent>,
    channel: ChannelId,
    needs: ThreadId,
    working: ThreadId,
    quiet: ThreadId,
    old: ThreadId,
    forge: AgentId,
}

const MONTH_MS: i64 = 30 * 24 * 60 * 60 * 1000;

fn busy(cx: &mut TestAppContext) -> (Busy, AnyWindowHandle, Entity<Workspace>) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let claude = busy_claude(tmp.path());
    let daemon = Daemon::start_with(home.path().to_path_buf(), claude.clone());
    let w: World = world(home.path(), &tmp.path().join("repo"), &["Forge", "Scout"]);
    let old: ThreadId = w.ask(Request::CreateThread {
        channel: w.channel,
        title: "old notes".into(),
    });
    let (channel, quiet, forge, scout) = (w.channel, w.thread, w.agents[0], w.agents[1]);
    drop(w);
    drop(daemon);

    // Last touched a month ago.
    let db = rusqlite::Connection::open(home.path().join("crew.db")).unwrap();
    let month_ago = super::common::now_ms() - MONTH_MS;
    db.execute(
        "UPDATE threads SET created_at = ?1 WHERE id = ?2",
        rusqlite::params![month_ago, old.to_string()],
    )
    .unwrap();
    drop(db);

    let daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let (client, events) = client::spawn(ConnectionConfig {
        crew_home: home.path().to_path_buf(),
    });
    let working: ThreadId = ask(
        &client,
        Request::CreateThread {
            channel,
            title: "write the docs".into(),
        },
    );
    let needs: ThreadId = ask(
        &client,
        Request::CreateThread {
            channel,
            title: "bump the deps".into(),
        },
    );
    // Tall enough to show every section.
    let (window, ws) = open_tall(cx, home.path());
    let _: MessageId = ask(
        &client,
        Request::PostMessage {
            thread: working,
            body: "@Forge write them".into(),
        },
    );
    let _: MessageId = ask(
        &client,
        Request::PostMessage {
            thread: needs,
            body: "@Scout bump them".into(),
        },
    );
    run_of(cx, &ws, working, forge, RunStatus::Working);
    let scout_run = run_of(cx, &ws, needs, scout, RunStatus::Working).id;
    let busy = Busy {
        _tmp: tmp,
        _home: home,
        _daemon: daemon,
        client,
        _events: events,
        channel,
        needs,
        working,
        quiet,
        old,
        forge,
    };
    // Answered only when the test ends.
    ask_in_background(&busy.client, scout_run);
    wait_until(cx, "the ask", |cx| {
        ws.read_with(cx, |ws, _| !ws.state().pending_approvals(needs).is_empty())
    });
    go_to_channel(cx, window, channel);
    (busy, window, ws)
}

/// Asks to use Bash in `run`, as crew-mcp would, without waiting for
/// the answer.
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

#[gpui_kit::test]
fn sections_go_needs_you_working_recent_older_with_older_folded(cx: &mut TestAppContext) {
    let (b, window, _ws) = busy(cx);
    wait_label(cx, window, "channel-section-needs".into(), "Needs you 1");
    wait_label(cx, window, "channel-section-working".into(), "Working 1");
    wait_label(cx, window, "channel-section-recent".into(), "Recent 1");
    wait_label(
        cx,
        window,
        "channel-section-older".into(),
        "Older 1 · last activity over 7 days ago",
    );
    let tops: Vec<f32> = ["needs", "working", "recent", "older"]
        .iter()
        .map(|s| top(cx, window, format!("channel-section-{s}")))
        .collect();
    assert!(tops.windows(2).all(|w| w[0] < w[1]), "in order: {tops:?}");
    wait_label(
        cx,
        window,
        format!("thread-status-{}", b.needs),
        "Needs you",
    );
    wait_label(
        cx,
        window,
        format!("thread-status-{}", b.working),
        "Working",
    );
    assert_eq!(
        label_of(cx, window, format!("thread-status-{}", b.quiet)).as_deref(),
        Some("Idle")
    );
    assert!(top(cx, window, card(b.needs)) < top(cx, window, card(b.working)));
    assert!(has(cx, window, card(b.quiet)));
    assert!(!has(cx, window, card(b.old)), "Older starts folded");
    assert_eq!(
        label_of(cx, window, format!("thread-excerpt-{}", b.working)).as_deref(),
        Some("Forge is working.")
    );
    wait_until(cx, "the running time", |cx| {
        label_of(cx, window, format!("thread-counts-{}", b.working))
            .is_some_and(|l| l.contains(" · running "))
    });
    wait_label(cx, window, "channel-stat-active".into(), "Active now 2");
    wait_label(cx, window, "channel-stat-needs".into(), "Needs you 1");

    click(cx, window, "channel-section-older");
    assert!(has(cx, window, card(b.old)));
    click(cx, window, "channel-section-recent");
    assert!(!has(cx, window, card(b.quiet)), "folded by hand");
}

#[gpui_kit::test]
fn the_working_filter_shows_only_working_threads(cx: &mut TestAppContext) {
    let (b, window, _ws) = busy(cx);
    wait_label(cx, window, "channel-filter-all".into(), "All 4");
    wait_label(cx, window, "channel-filter-working".into(), "Working 1");
    wait_label(cx, window, "channel-filter-needs".into(), "Needs you 1");
    assert_eq!(
        label_of(cx, window, "channel-filter-done").as_deref(),
        Some("Done 0")
    );
    assert_eq!(
        label_of(cx, window, "channel-filter-archived").as_deref(),
        Some("Archived 0")
    );
    click(cx, window, "channel-filter-working");
    assert!(has(cx, window, card(b.working)));
    for other in [b.needs, b.quiet, b.old] {
        assert!(!has(cx, window, card(other)));
    }
    assert!(!has(cx, window, "channel-section-working"), "a flat list");
    click(cx, window, "channel-filter-all");
    assert!(has(cx, window, card(b.quiet)));
}

#[gpui_kit::test]
fn search_narrows_the_cards(cx: &mut TestAppContext) {
    let (b, window, _ws) = busy(cx);
    wait_until(cx, "the cards", |cx| has(cx, window, card(b.quiet)));
    click(cx, window, "channel-search");
    type_text(cx, window, "LOGIN");
    wait_until(cx, "the search", |cx| !has(cx, window, card(b.working)));
    assert!(has(cx, window, card(b.quiet)));
    assert!(!has(cx, window, card(b.needs)));
    assert!(!has(cx, window, "channel-section-working"));
}

#[gpui_kit::test]
fn sort_by_created_puts_the_newest_thread_first(cx: &mut TestAppContext) {
    let (b, window, _ws) = busy(cx);
    // Newer than the quiet thread, but with no activity since.
    let newer: ThreadId = ask(
        &b.client,
        Request::CreateThread {
            channel: b.channel,
            title: "add search".into(),
        },
    );
    // Something happens in the older one, a moment later so the times
    // can't tie.
    std::thread::sleep(Duration::from_millis(20));
    let _: MessageId = ask(
        &b.client,
        Request::PostMessage {
            thread: b.quiet,
            body: "a note for later".into(),
        },
    );
    wait_until(cx, "latest activity first", |cx| {
        // The view asks crewd again a while after a change.
        cx.executor().advance_clock(Duration::from_millis(600));
        has(cx, window, card(newer))
            && has(cx, window, card(b.quiet))
            && left(cx, window, card(b.quiet)) < left(cx, window, card(newer))
    });
    assert_eq!(
        label_of(cx, window, "channel-sort-value").as_deref(),
        Some("Latest activity")
    );
    click(cx, window, "channel-sort");
    assert!(has(cx, window, "popup-menu"), "the sort menu");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(1usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    wait_label(cx, window, "channel-sort-value".into(), "Created");
    assert!(left(cx, window, card(newer)) < left(cx, window, card(b.quiet)));
}

#[gpui_kit::test]
fn stop_on_a_hovered_card_stops_its_run(cx: &mut TestAppContext) {
    let (b, window, ws) = busy(cx);
    let t = b.working;
    wait_until(cx, "the working card", |cx| has(cx, window, card(t)));
    hover(cx, window, card(t));
    assert!(has(cx, window, format!("thread-stop-{t}")));
    click(cx, window, format!("thread-stop-{t}"));
    run_of(cx, &ws, t, b.forge, RunStatus::Stopped);
    assert_eq!(ws.read_with(cx, |ws, _| ws.open_thread_id()), None, "stays");
    wait_label(cx, window, format!("thread-status-{t}"), "Stopped");
}

#[gpui_kit::test]
fn the_running_button_says_nothing_running_and_opens_running_now(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let _w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, _ws) = open(cx, home.path());
    wait_label(cx, window, "title-running".into(), "Nothing running");
    click(cx, window, "title-running");
    wait_to_show(cx, window, "Running now");
    assert!(has(cx, window, "running"));
}

#[gpui_kit::test]
fn archived_threads_show_only_under_archived(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    go_to_channel(cx, window, w.channel);
    wait_until(cx, "the card", |cx| has(cx, window, card(w.thread)));
    let () = w.ask(Request::ArchiveThread {
        thread: w.thread,
        archived: true,
    });
    wait_until(cx, "the card to go", |cx| !has(cx, window, card(w.thread)));
    wait_label(cx, window, "channel-filter-archived".into(), "Archived 1");
    ws.read_with(cx, |ws, _| {
        assert!(ws.state().threads(w.channel).is_empty());
    });
    click(cx, window, "channel-filter-archived");
    assert!(has(cx, window, card(w.thread)));
    let () = w.ask(Request::ArchiveThread {
        thread: w.thread,
        archived: false,
    });
    wait_until(cx, "the card to leave Archived", |cx| {
        !has(cx, window, card(w.thread))
    });
    click(cx, window, "channel-filter-all");
    assert!(has(cx, window, card(w.thread)));
}

#[gpui_kit::test]
fn counts_crewd_finishes_later_land_on_the_card(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let claude = fake_claude(
        tmp.path(),
        &format!(
            "printf 'hello\\nthere\\n' > README.md\nprintf 'one\\ntwo\\n' > new.txt\ncat '{}'",
            fixture("edit_and_test.jsonl").display()
        ),
    );
    let home = short_home();
    // Each count takes 2 s, past the 1.5 s crewd waits before answering.
    let _daemon = Daemon::start_with_options(
        home.path().to_path_buf(),
        crewd::ServeOptions {
            claude_bin: Some(claude),
            count_delay: Duration::from_secs(2),
            ..Default::default()
        },
    );
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
    let started = std::time::Instant::now();
    go_to_channel(cx, window, w.channel);

    let counts = format!("thread-counts-{}", w.thread);
    let has_counts = |cx: &mut TestAppContext| {
        label_of(cx, window, counts.clone()).is_some_and(|l| l.contains(" · 2 files +3 −1 · "))
    };
    // The first answer comes while crewd is still counting.
    wait_until(cx, "the first overview", |cx| {
        label_of(cx, window, counts.clone()).is_some()
    });
    assert!(!has_counts(cx));
    // Time in the view stands still until crewd had real time to finish
    // and say so. Then one refetch delay is enough: the 3 s fallback
    // never comes into it.
    let mut stepped = Duration::ZERO;
    wait_until(cx, "the counts crewd worked out", |cx| {
        if started.elapsed() > Duration::from_millis(3000) {
            cx.executor().advance_clock(Duration::from_millis(600));
            stepped += Duration::from_millis(600);
        }
        has_counts(cx)
    });
    assert!(stepped < Duration::from_secs(3), "{stepped:?}");
    assert_eq!(
        label_of(cx, window, format!("thread-worktree-{}", w.thread)).as_deref(),
        Some("2 uncommitted")
    );
}
