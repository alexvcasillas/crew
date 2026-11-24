//! End to end: the appearance settings reach the whole window, and the
//! defaults keep the original look.

use crew_core::ids::MessageId;
use crew_core::store::Author;
use gpui_kit::{AnyWindowHandle, AppContext as _, Bounds, Entity, Pixels, TestAppContext};

use super::common::font_probe;
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
    let text = |key: String| font_probe::size_of(&key).unwrap();
    assert_eq!(text(format!("message-body-{message}")), gpui_kit::px(13.));
    assert_eq!(
        text(format!("channel-label-{}", w.channel)),
        gpui_kit::px(13.)
    );

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
    let label_id = format!("channel-label-{}", w.channel);
    let send_before = bounds(cx, window, "composer-send".into()).size;
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

    let grown = 18. / 13.;
    assert_eq!(
        font_probe::size_of(&body_id),
        Some(gpui_kit::px(18.)),
        "message text"
    );
    assert_eq!(
        font_probe::size_of(&label_id),
        Some(gpui_kit::px(18.)),
        "sidebar row label"
    );
    let body = bounds(cx, window, body_id);
    assert!(
        near(body.size.height, 20. * grown),
        "message line: {:?}",
        body.size.height
    );
    let channel = bounds(cx, window, channel_id);
    assert_eq!(channel.size.height, gpui_kit::px(39.), "sidebar row");
    // A Kit button's label and padding are in rems, drawn on device pixels.
    let send = bounds(cx, window, "composer-send".into()).size;
    for (after, before) in [
        (send.width, send_before.width),
        (send.height, send_before.height),
    ] {
        assert!(
            (f32::from(after) - f32::from(before) * grown).abs() <= 1.5,
            "kit button: {send:?} from {send_before:?}"
        );
    }
    // The frame grows a little: 264 × (1 + 5 × 0.045).
    let sidebar = bounds(cx, window, "sidebar".into());
    assert_eq!(sidebar.size.width, gpui_kit::px(323.));

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
    press(cx, window, "cmd-+");
    assert_eq!(zoom(cx), 120, "⌘+ zooms in like ⌘=");
    press(cx, window, "cmd-- cmd-- cmd-- cmd--");
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
    let title: Vec<Bounds<Pixels>> = [
        "title-app",
        "title-search",
        "title-running",
        "title-settings",
    ]
    .into_iter()
    .map(|id| bounds(cx, window, id.into()))
    .collect();
    for pair in title.windows(2) {
        assert!(
            pair[0].right() <= pair[1].left(),
            "title bar items overlap: {title:?}"
        );
    }
    assert!(title[3].right() <= width, "settings off screen: {title:?}");
    assert!(
        title[0].size.width > gpui_kit::px(0.),
        "Crew shows: {title:?}"
    );

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

/// Settings › Appearance, open over a thread with one message of yours.
fn appearance_page(
    cx: &mut TestAppContext,
    home: &std::path::Path,
    repo: &std::path::Path,
) -> (AnyWindowHandle, Entity<Workspace>, MessageId) {
    let (_w, window, ws, message) = thread_with_message(cx, home, repo);
    open_page(cx, window);
    (window, ws, message)
}

fn open_page(cx: &mut TestAppContext, window: AnyWindowHandle) {
    super::test_ui::click(cx, window, "title-settings");
    super::test_ui::click(cx, window, "settings-nav-appearance");
    wait_until(cx, "the appearance page", |cx| {
        super::test_ui::has(cx, window, "settings-appearance")
    });
}

fn label(cx: &mut TestAppContext, window: AnyWindowHandle, id: &str) -> String {
    super::test_ui::label_of(cx, window, id.to_string()).unwrap_or_else(|| panic!("no {id}"))
}

/// What the Interface and Code font selects show.
fn font_selects(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> (String, String) {
    ws.read_with(cx, |ws, cx| {
        let page = ws.appearance_page().expect("the appearance page");
        let shown = |s: &Entity<super::settings::appearance::FontSelect>| {
            s.read(cx)
                .selected_value()
                .map(|v| v.to_string())
                .unwrap_or_default()
        };
        (shown(&page.ui_font), shown(&page.code_font))
    })
}

fn current(cx: &mut TestAppContext) -> Appearance {
    cx.update(|cx| scale::appearance(cx))
}

#[gpui_kit::test]
fn the_appearance_page_shows_every_row_with_the_defaults(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let (window, ws, _) = appearance_page(cx, home.path(), &tmp.path().join("repo"));

    // Appearance comes second in the nav.
    let nav: Vec<f32> = [
        "settings-nav-general",
        "settings-nav-appearance",
        "settings-nav-agents",
    ]
    .into_iter()
    .map(|id| f32::from(bounds(cx, window, id.into()).origin.y))
    .collect();
    assert!(nav[0] < nav[1] && nav[1] < nav[2], "{nav:?}");
    assert!(!super::test_ui::has(
        cx,
        window,
        "settings-nav-notifications"
    ));

    for row in [
        "Interface text size",
        "Code text size",
        "Fonts",
        "Line height",
        "Text rendering",
        "Density",
        "Sidebar",
        "Zoom",
    ] {
        assert!(super::test_ui::shows(cx, window, row), "{row}");
    }
    // The rows in the board's order.
    let ys: Vec<f32> = [
        "ui-size",
        "code-size",
        "fonts",
        "line-height",
        "rendering",
        "density",
        "sidebar",
        "zoom",
    ]
    .into_iter()
    .map(|key| f32::from(bounds(cx, window, format!("appearance-row-{key}")).origin.y))
    .collect();
    assert!(ys.windows(2).all(|p| p[0] < p[1]), "{ys:?}");

    assert_eq!(label(cx, window, "appearance-ui-size-13"), "13 (selected)");
    assert_eq!(label(cx, window, "appearance-ui-size-16"), "16");
    assert_eq!(
        label(cx, window, "appearance-code-size-12"),
        "12 (selected)"
    );
    assert_eq!(
        label(cx, window, "appearance-line-comfortable"),
        "Comfortable (selected)"
    );
    assert_eq!(
        label(cx, window, "appearance-render-standard"),
        "Standard (selected)"
    );
    assert_eq!(
        label(cx, window, "appearance-density-comfortable"),
        "Comfortable (selected)"
    );
    assert_eq!(
        label(cx, window, "appearance-sidebar-projects"),
        "Projects (selected)"
    );
    assert_eq!(label(cx, window, "appearance-zoom-value"), "100%");
    assert_eq!(
        font_selects(cx, &ws),
        ("Geist".to_string(), "Geist Mono".to_string())
    );
    assert_eq!(
        label(cx, window, "appearance-preview-caption"),
        "13 · 12 mono · standard"
    );

    // Threads isn't built yet: it stays off.
    super::test_ui::click(cx, window, "appearance-sidebar-threads");
    assert_eq!(
        current(cx).sidebar_mode,
        crate::prefs::SidebarMode::Projects
    );
    assert_eq!(crate::prefs::read(home.path(), APPEARANCE_KEY), None);
}

#[gpui_kit::test]
fn picking_16_changes_the_preview_and_thread_messages(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let (window, _ws, message) = appearance_page(cx, home.path(), &tmp.path().join("repo"));
    assert_eq!(
        font_probe::size_of("message-body-preview"),
        Some(gpui_kit::px(13.))
    );

    super::test_ui::click(cx, window, "appearance-ui-size-16");
    assert_eq!(label(cx, window, "appearance-ui-size-16"), "16 (selected)");
    assert_eq!(
        label(cx, window, "appearance-preview-caption"),
        "16 · 12 mono · standard"
    );
    assert_eq!(
        font_probe::size_of("message-body-preview"),
        Some(gpui_kit::px(16.))
    );
    super::test_ui::click(cx, window, "appearance-code-size-14");
    assert_eq!(
        label(cx, window, "appearance-preview-caption"),
        "16 · 14 mono · standard"
    );
    wait_until(cx, "the settings saved", |_| {
        let saved = Appearance::load(home.path());
        saved.ui_size == 16 && saved.code_size == 14
    });

    press(cx, window, "escape");
    let body = format!("message-body-{message}");
    wait_until(cx, "the thread", |cx| {
        super::test_ui::has(cx, window, body.clone())
    });
    assert_eq!(font_probe::size_of(&body), Some(gpui_kit::px(16.)));
}

#[gpui_kit::test]
fn bolder_text_makes_every_weight_heavier(cx: &mut TestAppContext) {
    use gpui_kit::FontWeight;
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let (window, _ws, message) = appearance_page(cx, home.path(), &tmp.path().join("repo"));
    assert_eq!(
        font_probe::weight_of("message-body-preview"),
        Some(FontWeight::NORMAL)
    );

    super::test_ui::click(cx, window, "appearance-render-bolder");
    assert_eq!(
        label(cx, window, "appearance-render-bolder"),
        "Bolder text (selected)"
    );
    assert!(super::test_ui::shows(
        cx,
        window,
        "Heavier strokes, easier to read on external displays."
    ));
    assert_eq!(
        label(cx, window, "appearance-preview-caption"),
        "13 · 12 mono · bolder"
    );
    assert_eq!(
        font_probe::weight_of("message-body-preview"),
        Some(FontWeight::MEDIUM)
    );
    assert!(current(cx).bolder);

    press(cx, window, "escape");
    let body = format!("message-body-{message}");
    wait_until(cx, "the thread", |cx| {
        super::test_ui::has(cx, window, body.clone())
    });
    assert_eq!(font_probe::weight_of(&body), Some(FontWeight::MEDIUM));
}

#[gpui_kit::test]
fn other_installed_font_lists_the_fonts_and_applies_one(cx: &mut TestAppContext) {
    use super::settings::appearance::{FontSlot, OTHER_FONT};
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let (window, ws, _) = appearance_page(cx, home.path(), &tmp.path().join("repo"));
    cx.update(|cx| {
        scale::set_installed_fonts(
            vec!["Avenir Next".into(), "Helvetica".into(), "Menlo".into()],
            cx,
        )
    });
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.pick_font(FontSlot::Interface, OTHER_FONT, window, cx)
        })
    })
    .unwrap();
    cx.run_until_parked();
    wait_until(cx, "the font list", |cx| {
        super::test_ui::has(cx, window, "font-picker")
    });
    for name in ["Avenir Next", "Helvetica", "Menlo"] {
        assert!(super::test_ui::shows(cx, window, name), "{name}");
    }
    // Picking Other doesn't change the font yet.
    assert_eq!(current(cx).ui_font, "Geist");
    assert_eq!(font_selects(cx, &ws).0, "Geist");

    super::test_ui::type_text(cx, window, "helv");
    assert!(!super::test_ui::shows(cx, window, "Avenir Next"));
    assert_eq!(label(cx, window, "font-option-0"), "Helvetica");
    super::test_ui::click(cx, window, "font-option-0");

    assert!(!super::test_ui::has(cx, window, "font-picker"), "closed");
    assert_eq!(current(cx).ui_font, "Helvetica");
    assert_eq!(cx.update(|_| scale::ui_family()), "Helvetica");
    assert_eq!(font_selects(cx, &ws).0, "Helvetica");
    wait_until(cx, "the font saved", |_| {
        Appearance::load(home.path()).ui_font == "Helvetica"
    });

    // A code font from the presets applies at once.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.pick_font(FontSlot::Code, "Menlo", window, cx)
        })
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(current(cx).code_font, "Menlo");
    assert_eq!(cx.update(|_| scale::code_family()), "Menlo");
    assert_eq!(font_selects(cx, &ws).1, "Menlo");
}

#[gpui_kit::test]
fn reset_puts_back_the_defaults(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let _w = world(home.path(), &tmp.path().join("repo"), &[]);
    // Tall enough for every row at 120%.
    let (window, ws) = super::test_ui::open_tall(cx, home.path());
    open_page(cx, window);

    // From the bottom up, so each control is still on screen.
    super::test_ui::click(cx, window, "appearance-zoom-in");
    super::test_ui::click(cx, window, "appearance-zoom-in");
    assert_eq!(label(cx, window, "appearance-zoom-value"), "120%");
    super::test_ui::click(cx, window, "appearance-zoom-out");
    assert_eq!(label(cx, window, "appearance-zoom-value"), "110%");
    for id in [
        "appearance-density-compact",
        "appearance-render-bolder",
        "appearance-line-roomy",
        "appearance-code-size-15",
        "appearance-ui-size-18",
    ] {
        super::test_ui::click(cx, window, id);
    }
    let changed = current(cx);
    assert_eq!(
        (changed.ui_size, changed.code_size, changed.zoom),
        (18, 15, 110)
    );
    assert!(changed.bolder);
    assert_eq!(changed.line_height, crate::prefs::LineHeight::Roomy);
    assert_eq!(changed.density, crate::prefs::Density::Compact);
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.pick_font(
                super::settings::appearance::FontSlot::Code,
                "Menlo",
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();

    super::test_ui::click(cx, window, "appearance-reset");
    assert_eq!(current(cx), Appearance::default());
    assert_eq!(label(cx, window, "appearance-ui-size-13"), "13 (selected)");
    assert_eq!(label(cx, window, "appearance-zoom-value"), "100%");
    assert_eq!(
        font_selects(cx, &ws),
        ("Geist".to_string(), "Geist Mono".to_string())
    );
    wait_until(cx, "the defaults saved", |_| {
        Appearance::load(home.path()) == Appearance::default()
    });
}

#[gpui_kit::test]
fn a_font_no_longer_installed_says_so_and_falls_back(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let (_window, ws, _) = appearance_page(cx, home.path(), &tmp.path().join("repo"));
    cx.update(|cx| {
        scale::set_installed_fonts(vec!["Helvetica".into()], cx);
        scale::set_appearance(
            Appearance {
                ui_font: "Gone Sans".into(),
                code_font: "Gone Mono".into(),
                ..Appearance::default()
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        font_selects(cx, &ws),
        (
            "Gone Sans (not installed)".to_string(),
            "Gone Mono (not installed)".to_string()
        )
    );
    assert_eq!(cx.update(|_| scale::ui_family()), crate::fonts::ui_font());
    assert_eq!(
        cx.update(|_| scale::code_family()),
        crate::fonts::mono_font()
    );
}
