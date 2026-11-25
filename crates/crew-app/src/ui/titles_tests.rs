//! End to end: Settings › General's title rows and a generated title
//! coming in, against a real crewd with a fake claude.

use std::path::Path;

use crew_core::ids::ThreadId;
use crew_proto::{Request, TitleMode, TitleModel, TitleSettings, TitleSource};
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::settings::Page;
use super::settings::general::{CODEX_FALLBACK, cost_hint};
use super::sidebar::row_probe::row_of;
use super::test_ui::{click, description_of, has, label_of, shows, wait_until};
use super::thread_tests::{open_thread, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

const TITLE: &str = "Flaky parallel runner on Linux";

/// crewd with titles on and a claude whose title runs wait for
/// `title-go` in `tmp`, then name the thread [`TITLE`]. Agent runs read a
/// file and answer.
fn daemon(tmp: &Path, home: &Path) -> Daemon {
    let go = tmp.join("title-go");
    let claude = fake_claude(
        tmp,
        &format!(
            "for a in \"$@\"; do\n\
               if [ \"$a\" = --no-session-persistence ]; then\n\
                 while [ ! -f '{go}' ]; do sleep 0.02; done\n\
                 echo '{{\"type\":\"result\",\"is_error\":false,\"result\":\"{TITLE}\"}}'\n\
                 exit 0\n\
               fi\n\
             done\n\
             cat '{fixture}'",
            go = go.display(),
            fixture = fixture("read_file.jsonl").display(),
        ),
    );
    Daemon::start_with_options(
        home.to_path_buf(),
        crewd::ServeOptions {
            claude_bin: Some(claude),
            ..Default::default()
        },
    )
}

fn open_page(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>, page: Page) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_settings(page, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
}

fn set_titles(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    titles: TitleSettings,
) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.set_titles(titles, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
}

fn source(cx: &mut TestAppContext, ws: &Entity<Workspace>, thread: ThreadId) -> TitleSource {
    ws.read_with(cx, |ws, _| ws.state().thread(thread).unwrap().title_source)
}

#[gpui_kit::test]
fn the_general_page_sets_how_new_threads_get_titles(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);
    open_page(cx, window, &ws, Page::General);

    assert!(shows(cx, window, "Thread titles"));
    assert_eq!(
        label_of(cx, window, "general-titles-generated").as_deref(),
        Some("Generated (selected)")
    );
    assert_eq!(
        label_of(cx, window, "general-titles-first-line").as_deref(),
        Some("First line of the message")
    );
    assert!(shows(cx, window, &cost_hint(TitleModel::Haiku)));
    assert!(shows(cx, window, CODEX_FALLBACK));
    assert!(has(cx, window, "general-titles-model"));

    // Sonnet: the hint follows.
    set_titles(
        cx,
        window,
        &ws,
        TitleSettings {
            mode: TitleMode::Generated,
            model: TitleModel::Sonnet,
        },
    );
    assert!(shows(cx, window, &cost_hint(TitleModel::Sonnet)));

    // First line: crewd keeps it, and a new thread keeps its first line.
    click(cx, window, "general-titles-first-line");
    assert_eq!(
        label_of(cx, window, "general-titles-first-line").as_deref(),
        Some("First line of the message (selected)")
    );
    wait_until(cx, "crewd to take the setting", |cx| {
        ws.read_with(cx, |ws, _| ws.state().titles().mode == TitleMode::FirstLine)
    });
    std::fs::write(tmp.path().join("title-go"), "").unwrap();
    w.post("@Forge look at the parser");
    wait_until(cx, "Forge's run", |cx| {
        ws.read_with(cx, |ws, _| !ws.state().thread_runs(w.thread).is_empty())
    });
    assert_eq!(source(cx, &ws, w.thread), TitleSource::FirstLine);
}

#[gpui_kit::test]
fn a_title_shimmers_while_it_is_generated_then_comes_in(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = daemon(tmp.path(), home.path());
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_thread(cx, home.path(), &w);

    // The thread header.
    w.post("@Forge can you look at why the parallel runner flakes on linux sometimes");
    wait_until(cx, "the title to be generated", |cx| {
        source(cx, &ws, w.thread) == TitleSource::Generating
    });
    assert_eq!(
        description_of(cx, window, "thread-title").as_deref(),
        Some("Generating a title")
    );
    assert_eq!(
        label_of(cx, window, "thread-title").as_deref(),
        Some("fix the login"),
        "the first line shows meanwhile"
    );
    std::fs::write(tmp.path().join("title-go"), "").unwrap();
    // The first line cross-fades into the title: it is kept for that.
    let mut replaced = None;
    wait_until(cx, "the generated title", |cx| {
        let shown = label_of(cx, window, "thread-title").as_deref() == Some(TITLE);
        if shown {
            replaced = ws.read_with(cx, |ws, _| {
                ws.state().title_replaced(w.thread).map(String::from)
            });
        }
        shown
    });
    assert_eq!(replaced.as_deref(), Some("fix the login"));
    assert_eq!(description_of(cx, window, "thread-title"), None);
    assert_eq!(source(cx, &ws, w.thread), TitleSource::Generated);

    // A channel card.
    std::fs::remove_file(tmp.path().join("title-go")).unwrap();
    let second: ThreadId = w.ask(Request::CreateThread {
        channel: w.channel,
        title: "second one".into(),
        first_line: true,
    });
    click(cx, window, row_of(w.channel));
    let card = format!("thread-title-{second}");
    wait_until(cx, "the second card", |cx| has(cx, window, card.clone()));
    let _: crew_core::ids::MessageId = w.ask(Request::PostMessage {
        thread: second,
        body: "@Forge second one".into(),
        skill: None,
        model_override: None,
    });
    wait_until(cx, "the card's title to be generated", |cx| {
        description_of(cx, window, card.clone()).as_deref() == Some("Generating a title")
    });
    assert_eq!(
        label_of(cx, window, card.clone()).as_deref(),
        Some("second one")
    );
    std::fs::write(tmp.path().join("title-go"), "").unwrap();
    wait_until(cx, "the card's generated title", |cx| {
        label_of(cx, window, card.clone()).as_deref() == Some(TITLE)
    });
}
