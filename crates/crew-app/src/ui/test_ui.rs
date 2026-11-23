//! Helpers for the window tests: open the app on a Crew home and drive it
//! in real time, since crewd and the client run on threads of their own.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui_kit::test::{TestQueryExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, ElementId, Entity, Point, TestAppContext,
    WindowBounds, WindowOptions, px, size,
};

use super::workspace::Workspace;
use crate::connection::ConnectionConfig;

/// Generous, for a loaded CI runner.
pub const WAIT: Duration = Duration::from_secs(20);

/// A window as `app::run` opens it, pointed at `home`. The client's
/// thread wakes the test executor, so parking is allowed.
pub fn open(cx: &mut TestAppContext, home: &Path) -> (AnyWindowHandle, Entity<Workspace>) {
    open_sized(cx, home, 1440.)
}

/// Like [`open`], `width` px wide.
pub fn open_sized(
    cx: &mut TestAppContext,
    home: &Path,
    width: f32,
) -> (AnyWindowHandle, Entity<Workspace>) {
    open_sized_tall(cx, home, width, 900.)
}

/// Like [`open`], tall enough that long forms need no scrolling.
pub fn open_tall(cx: &mut TestAppContext, home: &Path) -> (AnyWindowHandle, Entity<Workspace>) {
    open_sized_tall(cx, home, 1440., 1800.)
}

fn open_sized_tall(
    cx: &mut TestAppContext,
    home: &Path,
    width: f32,
    height: f32,
) -> (AnyWindowHandle, Entity<Workspace>) {
    cx.executor().allow_parking();
    let conn = ConnectionConfig {
        crew_home: home.to_path_buf(),
    };
    let (window, ws) = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::app::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(width), px(height)),
            })),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Workspace::new(conn, window, cx))
        })
        .unwrap()
    });
    cx.run_until_parked();
    (window, ws)
}

/// Runs the app until `done` holds, in real time. Panics after [`WAIT`].
pub fn wait_until(
    cx: &mut TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    let started = Instant::now();
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn wait_to_show(cx: &mut TestAppContext, window: AnyWindowHandle, label: &str) {
    wait_until(cx, label, |cx| shows(cx, window, label));
}

/// Whether an element with the accessible label `label` is on screen.
pub fn shows(cx: &mut TestAppContext, window: AnyWindowHandle, label: &str) -> bool {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find_by_label(label).is_some()
    })
    .unwrap()
}

/// Whether an element with the accessible label `label` is inside the
/// element `scope`.
pub fn shows_within(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    scope: impl Into<ElementId>,
    label: &str,
) -> bool {
    let scope = scope.into();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        if window.try_find(scope.clone()).is_none() {
            return false;
        }
        window.within(scope).try_find_by_label(label).is_some()
    })
    .unwrap()
}

pub fn has(cx: &mut TestAppContext, window: AnyWindowHandle, id: impl Into<ElementId>) -> bool {
    let id = id.into();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

/// The accessible label of the element `id`, if it is on screen.
pub fn label_of(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    id: impl Into<ElementId>,
) -> Option<String> {
    let id = id.into();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .try_find(id)
            .and_then(|e| e.label().map(str::to_string))
    })
    .unwrap()
}

pub fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: impl Into<ElementId>) {
    let id = id.into();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// Types `text` into whatever has focus.
pub fn type_text(cx: &mut TestAppContext, window: AnyWindowHandle, text: &str) {
    cx.update_window(window, |_, window, cx| window.input(text, cx))
        .unwrap();
    cx.run_until_parked();
}

/// Presses each space-separated key, such as `"down enter"`.
pub fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &str) {
    for key in keys.split_whitespace() {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    }
}

pub fn notifications(cx: &mut TestAppContext, window: AnyWindowHandle) -> usize {
    use gpui_kit::component::WindowExt as _;
    cx.update_window(window, |_, window, cx| window.notifications(cx).len())
        .unwrap()
}
