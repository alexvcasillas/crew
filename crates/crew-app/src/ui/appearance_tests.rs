//! End to end: the appearance settings reach the whole window, and the
//! defaults keep the original look.

use crew_core::ids::MessageId;
use crew_core::store::Author;
use gpui_kit::{AnyWindowHandle, AppContext as _, Bounds, Entity, Pixels, TestAppContext};

use super::test_ui::{bounds_of, press, wait_until};
use super::thread_tests::{World, open_thread, world};
use super::workspace::Workspace;
use crate::prefs::{APPEARANCE_KEY, Appearance};
use crate::test_support::{Daemon, short_home};
use crate::theme::scale;

/// A thread with one message of yours, open in the window.
fn thread_with_message(
    cx: &mut TestAppContext,
    home: &std::path::Path,
    repo: &std::path::Path,
) -> (World, AnyWindowHandle, Entity<Workspace>, MessageId) {
    let w = world(home, repo, &["Forge"]);
    w.post("hello");
    let (window, ws) = open_thread(cx, home, &w);
    let mut message = None;
    wait_until(cx, "the message", |cx| {
        message = ws.read_with(cx, |ws, _| {
            ws.state()
                .messages(w.thread)
                .iter()
                .find(|m| m.author == Author::You)
                .map(|m| m.id)
        });
        message.is_some()
    });
    (w, window, ws, message.unwrap())
}

fn bounds(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) -> Bounds<Pixels> {
    let mut found = None;
    wait_until(cx, &id, |cx| {
        found = bounds_of(cx, window, id.clone());
        found.is_some()
    });
    found.unwrap()
}

/// Layout lands on device pixels, half a point apart.
fn near(a: Pixels, b: f32) -> bool {
    (f32::from(a) - b).abs() <= 0.5
}

#[gpui_kit::test]
fn defaults_keep_the_original_look(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let (w, window, _ws, message) = thread_with_message(cx, home.path(), &tmp.path().join("repo"));

    let sidebar = bounds(cx, window, "sidebar".into());
    assert_eq!(sidebar.origin.y, gpui_kit::px(48.), "title bar height");
    assert_eq!(sidebar.size.width, gpui_kit::px(264.), "sidebar width");
    let status = bounds(cx, window, "status-bar".into());
    assert_eq!(status.size.height, gpui_kit::px(28.), "status bar height");
    let channel = bounds(cx, window, format!("channel-{}", w.channel));
    assert_eq!(channel.size.height, gpui_kit::px(28.), "sidebar row height");
    let body = bounds(cx, window, format!("message-body-{message}"));
    assert_eq!(body.size.height, gpui_kit::px(20.), "a message line");
    let add = bounds(cx, window, "sidebar-projects-add".into());
    assert_eq!(add.size.height, gpui_kit::px(16.), "a kit button");

    // Nothing is written until you change a setting.
    assert_eq!(crate::prefs::read(home.path(), APPEARANCE_KEY), None);
}

#[gpui_kit::test]
fn bigger_text_reaches_messages_the_sidebar_and_kit_buttons(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let (w, window, _ws, message) = thread_with_message(cx, home.path(), &tmp.path().join("repo"));
    let channel_id = format!("channel-{}", w.channel);
    let body_id = format!("message-body-{message}");
    let kit_before = bounds(cx, window, "sidebar-projects-add".into())
        .size
        .height;

    cx.update(|cx| {
        scale::set_appearance(
            Appearance {
                ui_size: 18,
                ..Appearance::default()
            },
            cx,
        )
    });
    cx.run_until_parked();

    let body = bounds(cx, window, body_id);
    assert!(
        near(body.size.height, 20. * 18. / 13.),
        "message line: {:?}",
        body.size.height
    );
    let channel = bounds(cx, window, channel_id);
    assert_eq!(channel.size.height, gpui_kit::px(39.), "sidebar row");
    let kit = bounds(cx, window, "sidebar-projects-add".into())
        .size
        .height;
    // Kit sizes are in rems, drawn on whole pixels.
    assert!(
        (f32::from(kit) - f32::from(kit_before) * 18. / 13.).abs() <= 1.,
        "kit button: {kit:?} from {kit_before:?}"
    );
    // Layout lengths follow the zoom only.
    let sidebar = bounds(cx, window, "sidebar".into());
    assert_eq!(sidebar.size.width, gpui_kit::px(264.));

    wait_until(cx, "the settings saved", |_| {
        Appearance::load(home.path()).ui_size == 18
    });
}

#[gpui_kit::test]
fn zoom_shortcuts_work_outside_text_fields(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let (_w, window, ws, _) = thread_with_message(cx, home.path(), &tmp.path().join("repo"));
    let zoom = |cx: &mut TestAppContext| cx.update(|cx| scale::appearance(cx).zoom);

    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.composer.update(cx, |c, cx| c.focus(window, cx))
        });
    })
    .unwrap();
    press(cx, window, "cmd-=");
    press(cx, window, "cmd--");
    press(cx, window, "cmd--");
    assert_eq!(zoom(cx), 100, "typing keeps the zoom");

    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.focus_window(window, cx));
    })
    .unwrap();
    press(cx, window, "cmd-=");
    assert_eq!(zoom(cx), 110);
    let sidebar = bounds(cx, window, "sidebar".into());
    assert!(near(sidebar.size.width, 264. * 1.1), "{sidebar:?}");
    press(cx, window, "cmd-- cmd-- cmd--");
    assert_eq!(zoom(cx), 80);
    press(cx, window, "cmd-0");
    assert_eq!(zoom(cx), 100);
    let sidebar = bounds(cx, window, "sidebar".into());
    assert_eq!(sidebar.size.width, gpui_kit::px(264.));
}

#[gpui_kit::test]
fn channel_cards_lay_out_again_for_bigger_text(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, _ws) = super::test_ui::open(cx, home.path());
    let channel = format!("channel-{}", w.channel);
    bounds(cx, window, channel.clone());
    super::test_ui::click(cx, window, channel);
    let card = format!("thread-{}", w.thread);
    let before = bounds(cx, window, card.clone()).size.height;

    cx.update(|cx| {
        scale::set_appearance(
            Appearance {
                ui_size: 18,
                ..Appearance::default()
            },
            cx,
        )
    });
    cx.run_until_parked();
    let after = bounds(cx, window, card).size.height;
    assert!(after > before, "card {before:?} then {after:?}");
}

#[gpui_kit::test]
fn big_text_and_zoom_fit_the_smallest_window(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = super::test_ui::open_sized_tall(
        cx,
        home.path(),
        crate::app::WINDOW_MIN_WIDTH,
        crate::app::WINDOW_MIN_HEIGHT,
    );
    super::thread_tests::go_to(cx, window, &ws, w.channel, w.thread);
    cx.update(|cx| {
        scale::set_appearance(
            Appearance {
                ui_size: 18,
                zoom: 200,
                ..Appearance::default()
            },
            cx,
        )
    });
    cx.run_until_parked();

    let width = gpui_kit::px(crate::app::WINDOW_MIN_WIDTH);
    let title: Vec<Bounds<Pixels>> = ["title-search", "title-running", "title-settings"]
        .into_iter()
        .map(|id| bounds(cx, window, id.into()))
        .collect();
    for pair in title.windows(2) {
        assert!(
            pair[0].right() <= pair[1].left(),
            "title bar items overlap: {title:?}"
        );
    }
    assert!(title[2].right() <= width, "settings off screen: {title:?}");

    let header: Vec<Bounds<Pixels>> = ["toggle-activity", "thread-handoffs"]
        .into_iter()
        .map(|id| bounds(cx, window, id.into()))
        .collect();
    assert!(header[0].right() <= header[1].left(), "{header:?}");
    assert!(
        header[1].right() <= width,
        "handoffs off screen: {header:?}"
    );
    let composer = bounds(cx, window, "composer".into());
    let send = bounds(cx, window, "composer-send".into());
    assert!(send.right() <= composer.right(), "{send:?} in {composer:?}");
    let sidebar = bounds(cx, window, "sidebar".into());
    assert!(
        sidebar.right() <= composer.left(),
        "{sidebar:?} {composer:?}"
    );
}
