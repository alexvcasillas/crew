//! The window's root view: title bar, sidebar, main area and status bar.

use crew_core::ids::ThreadId;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::status_bar::StatusBar;
use crate::client::{self, ClientEvent, ClientHandle};
use crate::connection::{ConnectionConfig, ConnectionState};
use crate::fonts::ui_font;
use crate::model::State;
use crate::theme::color;

pub const SIDEBAR_WIDTH: f32 = 264.;

pub struct Workspace {
    pub(super) conn: ConnectionConfig,
    /// What crewd told the client, connection included.
    state: State,
    pub(super) client: ClientHandle,
    /// Takes in what the client reports.
    _client_events: Task<()>,
    /// Starting (or restarting) crewd.
    pub(super) starting: bool,
    status_bar: Entity<StatusBar>,
    focus: FocusHandle,
    /// Starting crewd and waiting for it to answer.
    pub(super) service_task: Option<Task<()>>,
    /// The last error shown as a notification.
    pub(super) last_error: Option<SharedString>,
}

impl Workspace {
    pub fn new(conn: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (client, events) = client::spawn(conn.clone());
        Self {
            conn,
            state: State::default(),
            client,
            _client_events: Self::take_in_events(events, window, cx),
            starting: false,
            status_bar: cx.new(|_| StatusBar::default()),
            focus: cx.focus_handle(),
            service_task: None,
            last_error: None,
        }
    }

    /// What the UI renders from.
    pub fn state(&self) -> &State {
        &self.state
    }

    pub(super) fn starting(&self) -> bool {
        self.starting
    }

    /// Applies the client's events as they come, a batch per update.
    fn take_in_events(
        mut events: UnboundedReceiver<ClientEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                if this.update(cx, |this, cx| this.apply(batch, cx)).is_err() {
                    break;
                }
            }
        })
    }

    fn apply(&mut self, events: Vec<ClientEvent>, cx: &mut Context<Self>) {
        for event in events {
            event.apply_to(&mut self.state);
        }
        let connection = self.state.connection().cloned();
        self.status_bar
            .update(cx, |bar, cx| bar.set_connection(connection, cx));
        cx.notify();
    }

    /// Shows `thread`'s events live: its log fills from crewd's backlog,
    /// now and after every reconnect.
    pub fn watch_thread(&mut self, thread: ThreadId) {
        self.state.watch(thread);
        self.client.watch_thread(thread);
    }

    /// Stops keeping `thread`'s events.
    pub fn unwatch_thread(&mut self, thread: ThreadId) {
        self.state.unwatch(thread);
        self.client.unwatch_thread(thread);
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
        match self.state.connection() {
            // Checking: nothing to show yet.
            None => div().into_any_element(),
            // Reconnecting keeps what is known on screen.
            Some(ConnectionState::Connected | ConnectionState::Reconnecting) => {
                render_nothing_selected().into_any_element()
            }
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
    use crate::client;
    use crate::connection::ConnectionConfig;
    use crate::test_support::{Daemon, refuse_hello, short_home};
    use crew_core::ids::{ChannelId, ProjectId, ThreadId};
    use crew_proto::Request;
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::test::{TestQueryExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Generous, for a loaded CI runner.
    const WAIT: Duration = Duration::from_secs(20);

    /// A window as `app::run` opens it, pointed at `home`. The client's
    /// thread wakes the test executor, so parking is allowed.
    fn open(cx: &mut TestAppContext, home: &Path) -> (AnyWindowHandle, Entity<Workspace>) {
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

    /// Runs the app until `done` holds, in real time. Panics after
    /// [`WAIT`].
    fn wait_until(
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

    fn wait_to_show(cx: &mut TestAppContext, window: AnyWindowHandle, label: &str) {
        wait_until(cx, label, |cx| shows(cx, window, label));
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
        wait_to_show(cx, window, "Not connected");
        assert!(shows(
            cx,
            window,
            "Crew's background service isn't running."
        ));
        assert!(has(cx, window, "connect-start"));
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
        wait_to_show(cx, window, "Not connected");
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
        wait_to_show(cx, window, "Service out of date");
        server.join().unwrap();
        assert!(shows(cx, window, "Crew's service is out of date."));
        assert!(has(cx, window, "connect-restart"));
        assert!(!has(cx, window, "connect-start"));
    }

    #[gpui_kit::test]
    fn connected_daemon_shows_the_workspace(cx: &mut TestAppContext) {
        let home = short_home();
        let _daemon = Daemon::start(home.path().to_path_buf());
        let (window, _) = open(cx, home.path());
        wait_to_show(cx, window, "Connected to crewd");
        assert!(shows(cx, window, "Nothing selected"));
        assert!(!has(cx, window, "connect-start"));
    }

    /// crewd goes away and something on its socket never answers: the
    /// client keeps trying, and the workspace stays up meanwhile.
    #[gpui_kit::test]
    fn a_lost_connection_keeps_the_workspace_while_reconnecting(cx: &mut TestAppContext) {
        let home = short_home();
        let daemon = Daemon::start(home.path().to_path_buf());
        let (window, _) = open(cx, home.path());
        wait_to_show(cx, window, "Connected to crewd");

        drop(daemon);
        let _silent =
            std::os::unix::net::UnixListener::bind(home.path().join("crewd.sock")).unwrap();
        wait_to_show(cx, window, "Reconnecting…");
        assert!(shows(cx, window, "Nothing selected"));
        assert!(!has(cx, window, "connect-start"));
        assert!(!shows(
            cx,
            window,
            "Crew's background service isn't running."
        ));
    }

    #[gpui_kit::test]
    fn workspace_reflects_a_thread_another_client_creates(cx: &mut TestAppContext) {
        let home = short_home();
        let _daemon = Daemon::start(home.path().to_path_buf());
        let (window, ws) = open(cx, home.path());
        wait_to_show(cx, window, "Connected to crewd");

        // Another app or the CLI, on its own connection.
        let (other, _other_events) = client::spawn(ConnectionConfig {
            crew_home: home.path().to_path_buf(),
        });
        let ask = |req: Request| futures::executor::block_on(other.request(req)).unwrap();
        let project: ProjectId = serde_json::from_value(ask(Request::CreateProject {
            name: "demo".into(),
            root: "/work/demo".into(),
            main_branch: "main".into(),
        }))
        .unwrap();
        let channel: ChannelId = serde_json::from_value(ask(Request::CreateChannel {
            project,
            name: "general".into(),
        }))
        .unwrap();
        let thread: ThreadId = serde_json::from_value(ask(Request::CreateThread {
            channel,
            title: "fix the login".into(),
        }))
        .unwrap();

        wait_until(cx, "the new thread", |cx| {
            ws.read_with(cx, |ws, _| ws.state().thread(thread).is_some())
        });
        ws.read_with(cx, |ws, _| {
            let state = ws.state();
            assert_eq!(state.projects().len(), 1);
            assert_eq!(state.channels(project).len(), 1);
            let titles: Vec<&str> = state
                .threads(channel)
                .iter()
                .map(|t| t.title.as_str())
                .collect();
            assert_eq!(titles, ["fix the login"]);
        });
    }
}
