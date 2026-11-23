//! The window's root view: title bar, sidebar, main area and status bar.

use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::status_bar::StatusBar;
use crate::connection::{self, ConnectionConfig, ConnectionState, StartError};
use crate::fonts::ui_font;
use crate::theme::color;

pub const SIDEBAR_WIDTH: f32 = 264.;
/// How long "Start Crew service" waits for crewd to answer.
const START_WAIT: Duration = Duration::from_secs(5);
/// How long "Restart service" waits for the old crewd to stop. Its runs
/// get up to 8 s to stop.
const STOP_WAIT: Duration = Duration::from_secs(10);

pub struct Workspace {
    conn: ConnectionConfig,
    /// `None` while the first check runs.
    connection: Option<ConnectionState>,
    /// Starting (or restarting) crewd.
    starting: bool,
    status_bar: Entity<StatusBar>,
    focus: FocusHandle,
    /// The running check, or start and check.
    probe: Option<Task<()>>,
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

    fn set_connection(&mut self, state: ConnectionState, cx: &mut Context<Self>) {
        self.connection = Some(state.clone());
        self.status_bar
            .update(cx, |bar, cx| bar.set_connection(Some(state), cx));
        cx.notify();
    }

    /// Starts crewd (stopping the running one first if `restart`), then
    /// checks until it answers or [`START_WAIT`] passes.
    pub(super) fn start_service(
        &mut self,
        restart: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.starting {
            return;
        }
        self.starting = true;
        cx.notify();
        let conn = self.conn.clone();
        let executor = cx.background_executor().clone();
        self.probe = Some(cx.spawn_in(window, async move |this, cx| {
            let start_conn = conn.clone();
            let started = executor
                .spawn(async move {
                    if restart && !connection::stop_crewd(&start_conn, STOP_WAIT) {
                        return Err(StartFailure::StillRunning);
                    }
                    connection::start_crewd(&start_conn).map_err(StartFailure::Start)
                })
                .await;
            let state = match started {
                Ok(()) => Ok(wait_for_crewd(&executor, &conn).await),
                Err(e) => Err(e),
            };
            this.update_in(cx, |this, window, cx| {
                this.starting = false;
                let state = match state {
                    Ok(state) => {
                        if state == ConnectionState::NotRunning {
                            let log = this.conn.log();
                            let msg = format!(
                                "Crew's service didn't start. Its log is at {}.",
                                log.display()
                            );
                            window.push_notification(Notification::error(msg), cx);
                        }
                        state
                    }
                    Err(e) => {
                        window.push_notification(Notification::error(e.message()), cx);
                        this.connection
                            .clone()
                            .unwrap_or(ConnectionState::NotRunning)
                    }
                };
                this.set_connection(state, cx);
            })
            .ok();
        }));
    }
}

/// Why "Start" or "Restart" failed before crewd could be checked.
enum StartFailure {
    StillRunning,
    Start(StartError),
}

impl StartFailure {
    fn message(&self) -> String {
        match self {
            StartFailure::StillRunning => {
                "Crew's old service didn't stop. Quit it, then try again.".into()
            }
            StartFailure::Start(StartError::NotFound) => {
                "crewd wasn't found next to Crew. Reinstall Crew.".into()
            }
            StartFailure::Start(StartError::Io(e)) => {
                format!("Crew's service couldn't start ({e}). Reinstall Crew.")
            }
        }
    }
}

/// Probes crewd with growing pauses until it answers or [`START_WAIT`]
/// passes.
async fn wait_for_crewd(executor: &BackgroundExecutor, conn: &ConnectionConfig) -> ConnectionState {
    let mut pause = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    loop {
        let probe_conn = conn.clone();
        let state = executor
            .spawn(async move { connection::probe(&probe_conn) })
            .await;
        if state != ConnectionState::NotRunning || waited >= START_WAIT {
            return state;
        }
        executor.timer(pause).await;
        waited += pause;
        pause = (pause * 2).min(Duration::from_secs(1));
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
    use gpui_kit::test::{TestQueryExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use std::path::{Path, PathBuf};
    use std::time::Duration;

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

    /// A short path, so the socket stays under the Unix path limit.
    fn short_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("crew")
            .tempdir_in("/tmp")
            .unwrap()
    }

    /// crewd on its own runtime thread, stopped and joined on drop.
    struct Daemon {
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Daemon {
        fn start(home: PathBuf) -> Self {
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            let serve_home = home.clone();
            let thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    let opts = crewd::ServeOptions {
                        crew_mcp_bin: None,
                        ..Default::default()
                    };
                    let shutdown = async move {
                        let _ = stopped.await;
                    };
                    crewd::serve_until(&serve_home, opts, shutdown)
                        .await
                        .unwrap();
                });
            });
            let socket = home.join("crewd.sock");
            for _ in 0..500 {
                if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                    return Self {
                        stop: Some(stop),
                        thread: Some(thread),
                    };
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("crewd did not start");
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            drop(self.stop.take());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[gpui_kit::test]
    fn window_opens_with_title_bar_and_status_bar(cx: &mut TestAppContext) {
        let home = tempfile::tempdir().unwrap();
        let (window, _) = open(cx, home.path());
        assert!(has(cx, window, "title-bar"));
        assert!(has(cx, window, "status-bar"));
        assert!(shows(cx, window, "Crew"), "the app name");
        assert!(shows(cx, window, "Search threads, agents, files…"));
        assert!(shows(cx, window, "Projects"), "the sidebar");
    }

    #[gpui_kit::test]
    fn no_daemon_shows_the_start_screen(cx: &mut TestAppContext) {
        let home = tempfile::tempdir().unwrap();
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
