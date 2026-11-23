//! The window's root view: title bar, sidebar, main area and status bar.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::status_bar::StatusBar;
use crate::connection::{self, ConnectionConfig, ConnectionState};
use crate::fonts::ui_font;
use crate::theme::color;

pub const SIDEBAR_WIDTH: f32 = 264.;

pub struct Workspace {
    pub(super) conn: ConnectionConfig,
    /// `None` while the first check runs.
    connection: Option<ConnectionState>,
    /// Starting (or restarting) crewd.
    pub(super) starting: bool,
    status_bar: Entity<StatusBar>,
    focus: FocusHandle,
    /// The running check, or start and check.
    pub(super) probe: Option<Task<()>>,
    /// The last error shown as a notification.
    pub(super) last_error: Option<SharedString>,
}

impl Workspace {
    pub fn new(conn: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut ws = Self {
            conn,
            connection: None,
            starting: false,
            status_bar: cx.new(|_| StatusBar::default()),
            focus: cx.focus_handle(),
            probe: None,
            last_error: None,
        };
        ws.check(window, cx);
        ws
    }

    pub(super) fn starting(&self) -> bool {
        self.starting
    }

    /// Checks whether crewd answers, off the UI thread.
    fn check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let probe = cx
            .background_executor()
            .spawn(async move { connection::probe(&conn) });
        self.probe = Some(cx.spawn_in(window, async move |this, cx| {
            let state = probe.await;
            this.update(cx, |this, cx| this.set_connection(state, cx))
                .ok();
        }));
    }

    pub(super) fn set_connection(&mut self, state: ConnectionState, cx: &mut Context<Self>) {
        self.connection = Some(state.clone());
        self.status_bar
            .update(cx, |bar, cx| bar.set_connection(Some(state), cx));
        cx.notify();
    }

    /// Shows `error` as a notification.
    pub(super) fn show_error(
        &mut self,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_error = Some(error.clone().into());
        window.push_notification(Notification::error(error), cx);
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(color("bg-100"))
            .text_color(color("text-primary"))
            .font_family(ui_font())
            .text_size(px(13.))
            .child(self.render_title_bar())
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(render_sidebar())
                    .child(
                        div()
                            .id("main")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_main(cx)),
                    ),
            )
            .child(self.status_bar.clone())
    }
}

impl Workspace {
    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.connection {
            // Checking: nothing to show yet.
            None => div().into_any_element(),
            Some(ConnectionState::Connected) => render_nothing_selected().into_any_element(),
            Some(state) => self.render_connect(state, cx).into_any_element(),
        }
    }
}

fn render_sidebar() -> impl IntoElement {
    div()
        .id("sidebar")
        .flex()
        .flex_col()
        .flex_none()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .px(px(8.))
        .py(px(12.))
        .border_r_1()
        .border_color(color("border-100"))
        .child(
            div()
                .id("sidebar-projects")
                .px(px(8.))
                .text_size(px(11.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color("text-tertiary"))
                .aria_label("Projects")
                .child("Projects")
                .test_support(),
        )
}

fn render_nothing_selected() -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .id("nothing-selected")
                .text_color(color("text-tertiary"))
                .aria_label("Nothing selected")
                .child("Nothing selected")
                .test_support(),
        )
}

#[cfg(test)]
mod gpui_tests {
    use super::Workspace;
    use crate::connection::ConnectionConfig;
    use crate::test_support::{Daemon, refuse_hello, short_home};
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::test::{TestQueryExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use std::path::Path;

    /// A window as `app::run` opens it, pointed at `home`.
    fn open(cx: &mut TestAppContext, home: &Path) -> (AnyWindowHandle, Entity<Workspace>) {
        let conn = ConnectionConfig {
            crew_home: home.to_path_buf(),
        };
        let (window, ws) = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::app::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1440.), px(900.)),
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

    fn shows(cx: &mut TestAppContext, window: AnyWindowHandle, label: &str) -> bool {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find_by_label(label).is_some()
        })
        .unwrap()
    }

    fn has(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) -> bool {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn notifications(cx: &mut TestAppContext, window: AnyWindowHandle) -> usize {
        cx.update_window(window, |_, window, cx| window.notifications(cx).len())
            .unwrap()
    }

    #[gpui_kit::test]
    fn window_opens_with_title_bar_and_status_bar(cx: &mut TestAppContext) {
        let home = short_home();
        let (window, _) = open(cx, home.path());
        assert!(has(cx, window, "title-bar"));
        assert!(has(cx, window, "status-bar"));
        assert!(shows(cx, window, "Crew"), "the app name");
        assert!(shows(cx, window, "Search threads, agents, files…"));
        assert!(shows(cx, window, "Projects"), "the sidebar");
    }

    #[gpui_kit::test]
    fn no_daemon_shows_the_start_screen(cx: &mut TestAppContext) {
        let home = short_home();
        let (window, _) = open(cx, home.path());
        assert!(shows(
            cx,
            window,
            "Crew's background service isn't running."
        ));
        assert!(has(cx, window, "connect-start"));
        assert!(shows(cx, window, "Not connected"));
        assert!(!shows(cx, window, "Nothing selected"));
    }

    /// The test binary lives in `target/*/deps`, where there is no file
    /// named `crewd`.
    #[gpui_kit::test]
    fn start_without_crewd_beside_the_app_says_to_reinstall(cx: &mut TestAppContext) {
        let exe = std::env::current_exe().unwrap();
        assert!(!exe.with_file_name("crewd").exists());
        let home = short_home();
        let (window, ws) = open(cx, home.path());
        assert_eq!(notifications(cx, window), 0);
        click(cx, window, "connect-start");
        assert_eq!(notifications(cx, window), 1);
        ws.read_with(cx, |ws, _| {
            assert_eq!(
                ws.last_error.as_deref(),
                Some("crewd wasn't found next to Crew. Reinstall Crew.")
            );
            assert!(!ws.starting);
        });
        assert!(has(cx, window, "connect-start"));
        assert!(shows(cx, window, "Not connected"));
    }

    #[gpui_kit::test]
    fn out_of_date_daemon_offers_a_restart(cx: &mut TestAppContext) {
        let home = short_home();
        let server = refuse_hello(&home.path().join("crewd.sock"), 2);
        let (window, _) = open(cx, home.path());
        server.join().unwrap();
        assert!(shows(cx, window, "Crew's service is out of date."));
        assert!(has(cx, window, "connect-restart"));
        assert!(!has(cx, window, "connect-start"));
        assert!(shows(cx, window, "Service out of date"));
    }

    #[gpui_kit::test]
    fn connected_daemon_shows_the_workspace(cx: &mut TestAppContext) {
        let home = short_home();
        let _daemon = Daemon::start(home.path().to_path_buf());
        let (window, _) = open(cx, home.path());
        assert!(shows(cx, window, "Connected to crewd"));
        assert!(shows(cx, window, "Nothing selected"));
        assert!(!has(cx, window, "connect-start"));
    }
}
