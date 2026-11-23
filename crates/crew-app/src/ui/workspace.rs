//! The window's root view: title bar, sidebar, main area and status bar.
//! The views themselves live in their own files; this one holds what is
//! selected and talks to the client.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crew_core::event::RunStatus;
use crew_core::ids::{ApprovalId, ChannelId, ProjectId, RunId, ThreadId};
use crew_proto::Request;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::approval_card::BriefEditor;
use super::common::runtime_chip;
use super::composer::{Composer, ComposerEvent};
use super::inspector::Inspector;
use super::mention_menu::MentionAgent;
use super::status_bar::StatusBar;
use crate::app::{ApproveLatest, NewThread};
use crate::client::{self, ClientEvent, ClientHandle};
use crate::connection::{ConnectionConfig, ConnectionState};
use crate::fonts::ui_font;
use crate::model::State;
use crate::theme::color;

pub const SIDEBAR_WIDTH: f32 = 264.;
/// Longest thread title taken from a first message.
pub const TITLE_MAX: usize = 80;

/// What is selected and open, kept in memory only.
#[derive(Default)]
pub(super) struct Nav {
    /// Projects whose channels are hidden.
    pub(super) collapsed: HashSet<ProjectId>,
    pub(super) channel: Option<ChannelId>,
    /// The open, watched thread.
    pub(super) thread: Option<ThreadId>,
    /// Writing the first message of a new thread in `channel`.
    pub(super) drafting: bool,
    /// When you last looked at each channel, in Unix ms.
    pub(super) viewed: HashMap<ChannelId, i64>,
    pub(super) scroll: ScrollHandle,
    /// Messages in the open thread when last drawn, to scroll on new ones.
    seen_messages: usize,
    /// The thread was just opened: show its end whatever the scroll.
    stick_to_bottom: bool,
}

/// Whose text the composer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum DraftKey {
    Thread(ThreadId),
    /// The first message of a new thread in the channel.
    NewThread(ChannelId),
}

/// How close to the end, in px, still counts as reading the end.
const NEAR_BOTTOM: f32 = 40.;

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
    pub(super) composer: Entity<Composer>,
    _composer_events: Subscription,
    focus: FocusHandle,
    pub(super) nav: Nav,
    /// What you wrote and left, per thread and per new-thread draft.
    drafts: HashMap<DraftKey, String>,
    /// The channel whose new thread is being created.
    creating: Option<ChannelId>,
    /// A thread to open once it is known (debug builds' startup hook).
    open_pending: Option<ThreadId>,
    /// Redraws the open thread's timers once a second while a run works.
    ticker: Option<Task<()>>,
    /// Requests waiting for their answers, by a number of their own.
    requests: HashMap<u64, Task<()>>,
    next_request: u64,
    /// Starting crewd and waiting for it to answer.
    pub(super) service_task: Option<Task<()>>,
    /// The last error shown as a notification.
    pub(super) last_error: Option<SharedString>,
    pub(super) inspector: Inspector,
    /// The handoff brief you are editing, if any.
    pub(super) editing: Option<BriefEditor>,
    /// Permission asks whose whole input is shown.
    pub(super) open_inputs: HashSet<ApprovalId>,
    /// Approvals whose answer was sent.
    pub(super) deciding: HashSet<ApprovalId>,
}

impl Workspace {
    pub fn new(conn: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (client, events) = client::spawn(conn.clone());
        let composer = cx.new(|cx| Composer::new(window, cx));
        let composer_events =
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    ComposerEvent::Send(text) => this.send(text.clone(), window, cx),
                },
            );
        Self {
            conn,
            state: State::default(),
            client,
            _client_events: Self::take_in_events(events, window, cx),
            starting: false,
            status_bar: cx.new(|_| StatusBar::default()),
            composer,
            _composer_events: composer_events,
            focus: cx.focus_handle(),
            nav: Nav::default(),
            drafts: HashMap::new(),
            creating: None,
            open_pending: dev_open_thread(),
            ticker: None,
            requests: HashMap::new(),
            next_request: 0,
            service_task: None,
            last_error: None,
            inspector: Inspector::default(),
            editing: None,
            open_inputs: HashSet::new(),
            deciding: HashSet::new(),
        }
    }

    /// What the UI renders from.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Opens `thread` now if it is known, else as soon as it arrives, as
    /// the debug startup hook does.
    #[cfg(test)]
    pub(super) fn open_when_known(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_pending = Some(thread);
        self.open_if_known(window, cx);
    }

    fn open_if_known(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.open_pending
            && self.state.thread(thread).is_some()
        {
            self.open_pending = None;
            self.open_thread(thread, window, cx);
        }
    }

    /// The open thread, if any.
    pub fn open_thread_id(&self) -> Option<ThreadId> {
        self.nav.thread
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
                let applied = this.update_in(cx, |this, window, cx| this.apply(batch, window, cx));
                if applied.is_err() {
                    break;
                }
            }
        })
    }

    fn apply(&mut self, events: Vec<ClientEvent>, window: &mut Window, cx: &mut Context<Self>) {
        for event in events {
            event.apply_to(&mut self.state);
        }
        let connection = self.state.connection().cloned();
        self.status_bar
            .update(cx, |bar, cx| bar.set_connection(connection, cx));
        self.sync(cx);
        self.sync_inspector(window, cx);
        self.open_if_known(window, cx);
        cx.notify();
    }

    /// Brings the composer, the timer and the scroll position in line
    /// with what is known and open.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let project = self
            .nav
            .channel
            .and_then(|c| self.state.channel(c))
            .and_then(|c| self.state.project(c.project))
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let agents: Vec<MentionAgent> = self
            .state
            .agents()
            .into_iter()
            .map(|a| MentionAgent {
                name: a.spec.name.clone(),
                role: a.spec.role.clone(),
                chip: runtime_chip(&a.spec),
                status: self.state.agent_status(a.id),
            })
            .collect();
        let working: Vec<String> = self
            .nav
            .thread
            .map(|t| self.state.live_runs(t))
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.status == RunStatus::Working)
            .filter_map(|r| self.state.agent(r.agent).map(|a| a.spec.name.clone()))
            .collect();
        let hint = working_hint(&working);
        let busy =
            self.nav.drafting && self.creating.is_some() && self.creating == self.nav.channel;
        self.composer.update(cx, |composer, cx| {
            composer.set_context(agents, project, hint, cx);
            composer.set_busy(busy, cx);
        });

        if working.is_empty() {
            self.ticker = None;
        } else if self.ticker.is_none() {
            self.ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }

        let messages = self.nav.thread.map_or(0, |t| self.state.messages(t).len());
        if messages != self.nav.seen_messages {
            self.nav.seen_messages = messages;
            // The scroll handle still measures the content drawn last.
            let scroll = &self.nav.scroll;
            let at_end = near_bottom(scroll.offset().y.as_f32(), scroll.max_offset().y.as_f32());
            if std::mem::take(&mut self.nav.stick_to_bottom) || at_end {
                scroll.scroll_to_bottom();
            }
        }
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

    /// Shows the channel's threads.
    pub(super) fn select_channel(
        &mut self,
        channel: ChannelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stash_draft(cx);
        self.close_thread();
        self.nav.drafting = false;
        self.look_at(Some(channel));
        self.composer.update(cx, |composer, cx| {
            composer.set_first_message(false, window, cx)
        });
        self.load_draft(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// Opens `thread` and watches it, in place of the open one.
    pub(super) fn open_thread(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stash_draft(cx);
        if self.nav.thread != Some(thread) {
            self.close_thread();
            self.watch_thread(thread);
            self.nav.thread = Some(thread);
            self.nav.seen_messages = 0;
            self.nav.stick_to_bottom = true;
            self.sync_inspector(window, cx);
        }
        self.nav.drafting = false;
        let channel = self.state.thread(thread).map(|t| t.channel);
        self.look_at(channel.or(self.nav.channel));
        self.composer.update(cx, |composer, cx| {
            composer.set_first_message(false, window, cx);
            composer.focus(window, cx);
        });
        self.load_draft(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// Starts a new thread in the selected channel: the composer takes its
    /// first message.
    pub(super) fn new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.nav.channel.is_none() {
            return;
        }
        self.stash_draft(cx);
        self.close_thread();
        self.nav.drafting = true;
        self.composer.update(cx, |composer, cx| {
            composer.set_first_message(true, window, cx);
            composer.focus(window, cx);
        });
        self.load_draft(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// Whose text the composer holds now.
    fn draft_key(&self) -> Option<DraftKey> {
        match (self.nav.thread, self.nav.channel) {
            (Some(thread), _) => Some(DraftKey::Thread(thread)),
            (None, Some(channel)) if self.nav.drafting => Some(DraftKey::NewThread(channel)),
            _ => None,
        }
    }

    /// Keeps the composer's text for what is open, before leaving it.
    fn stash_draft(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.draft_key() {
            let text = self.composer.read(cx).text(cx);
            if text.is_empty() {
                self.drafts.remove(&key);
            } else {
                self.drafts.insert(key, text);
            }
        }
    }

    /// Puts the text kept for what is now open in the composer.
    fn load_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .draft_key()
            .and_then(|key| self.drafts.remove(&key))
            .unwrap_or_default();
        self.composer
            .update(cx, |composer, cx| composer.set_text(&text, window, cx));
    }

    /// Gives `text` back to `key` after a send failed, unless you have
    /// written something else there since.
    fn restore_draft(
        &mut self,
        key: DraftKey,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.draft_key() == Some(key) {
            if self.composer.read(cx).text(cx).trim().is_empty() {
                self.composer
                    .update(cx, |composer, cx| composer.set_text(&text, window, cx));
            }
        } else {
            self.drafts.entry(key).or_insert(text);
        }
    }

    fn close_thread(&mut self) {
        if let Some(open) = self.nav.thread.take() {
            self.unwatch_thread(open);
            self.inspector.reset();
            self.editing = None;
        }
    }

    /// Marks the channel you leave and the one you go to as seen now.
    fn look_at(&mut self, channel: Option<ChannelId>) {
        let now = super::common::now_ms();
        if let Some(left) = self.nav.channel {
            self.nav.viewed.insert(left, now);
        }
        if let Some(channel) = channel {
            self.nav.viewed.insert(channel, now);
        }
        self.nav.channel = channel;
    }

    /// Posts what you wrote: in the open thread, or as the first message
    /// of a new one titled by its first line. A new thread opens when
    /// crewd made it, if you are still on its draft.
    fn send(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(key) = self.draft_key() else {
            return;
        };
        match key {
            DraftKey::NewThread(channel) => {
                if self.creating.is_some() {
                    return;
                }
                self.creating = Some(channel);
                self.sync(cx);
                let title = thread_title(&text);
                self.spawn_request(window, cx, async move |this, cx| {
                    let made = client
                        .request(Request::CreateThread { channel, title })
                        .await
                        .and_then(|v| {
                            serde_json::from_value::<ThreadId>(v).map_err(|e| e.to_string())
                        });
                    let still_here = this
                        .update_in(cx, |this, window, cx| {
                            this.creating = None;
                            let here = this.draft_key() == Some(key);
                            if let (Ok(thread), true) = (&made, here) {
                                this.open_thread(*thread, window, cx);
                            }
                            this.sync(cx);
                            here
                        })
                        .unwrap_or(false);
                    let thread = match made {
                        Ok(thread) => thread,
                        Err(e) => return Err((e, Some((key, text)))),
                    };
                    let posted = client
                        .request(Request::PostMessage {
                            thread,
                            body: text.clone(),
                        })
                        .await;
                    let back = if still_here {
                        DraftKey::Thread(thread)
                    } else {
                        key
                    };
                    posted.map(drop).map_err(|e| (e, Some((back, text))))
                });
            }
            DraftKey::Thread(thread) => {
                self.spawn_request(window, cx, async move |_, _| {
                    client
                        .request(Request::PostMessage {
                            thread,
                            body: text.clone(),
                        })
                        .await
                        .map(drop)
                        .map_err(|e| (e, Some((key, text))))
                });
            }
        }
    }

    pub(super) fn stop_run(&mut self, run: RunId, window: &mut Window, cx: &mut Context<Self>) {
        self.request(Request::StopRun { run }, window, cx);
    }

    /// Sends `req`, showing its error if it fails.
    pub(super) fn request(&mut self, req: Request, window: &mut Window, cx: &mut Context<Self>) {
        let answer = self.client.request(req);
        self.spawn_request(window, cx, async move |_, _| {
            answer.await.map(drop).map_err(|e| (e, None))
        });
    }

    /// Runs `work`, keeping its task until it ends. An error is shown,
    /// and its text, if any, goes back to the draft it came from.
    pub(super) fn spawn_request(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        work: impl AsyncFnOnce(
            WeakEntity<Self>,
            &mut AsyncWindowContext,
        ) -> Result<(), (String, Option<(DraftKey, String)>)>
        + 'static,
    ) {
        let id = self.next_request;
        self.next_request += 1;
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = work(this.clone(), cx).await;
            this.update_in(cx, |this, window, cx| {
                if let Err((error, text)) = result {
                    if let Some((key, text)) = text {
                        this.restore_draft(key, text, window, cx);
                    }
                    this.show_error(error, window, cx);
                }
                this.requests.remove(&id);
            })
            .ok();
        });
        self.requests.insert(id, task);
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

/// A thread's title from its first message: the first line, at most
/// [`TITLE_MAX`] characters.
pub fn thread_title(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .trim();
    line.chars().take(TITLE_MAX).collect()
}

/// Whether a scroll `offset` (negative as content moves up) is within
/// [`NEAR_BOTTOM`] of its end, `max`.
pub(super) fn near_bottom(offset: f32, max: f32) -> bool {
    -offset >= max - NEAR_BOTTOM
}

/// `CREW_APP_OPEN_THREAD`, in debug builds only.
fn dev_open_thread() -> Option<ThreadId> {
    #[cfg(debug_assertions)]
    {
        super::dev_hooks::read(|k| std::env::var(k).ok()).open_thread
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// "Forge is working. A reply now is queued for its next turn."
fn working_hint(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!(
            "{one} is working. A reply now is queued for its next turn."
        )),
        [rest @ .., last] => Some(format!(
            "{} and {last} are working. A reply now is queued for their next turns.",
            rest.join(", ")
        )),
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NewThread, window, cx| this.new_thread(window, cx)))
            .on_action(
                cx.listener(|this, _: &ApproveLatest, window, cx| this.approve_latest(window, cx)),
            )
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
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .id("main")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_main(cx)),
                    )
                    .children(
                        self.inspector_thread()
                            .map(|t| self.render_inspector(t, cx)),
                    ),
            )
            .child(self.status_bar.clone())
    }
}

impl Workspace {
    /// The thread the inspector shows: the open one, while connected.
    fn inspector_thread(&self) -> Option<ThreadId> {
        let shown = matches!(
            self.state.connection(),
            Some(ConnectionState::Connected | ConnectionState::Reconnecting)
        );
        self.nav
            .thread
            .filter(|t| shown && self.state.thread(*t).is_some())
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.state.connection() {
            // Checking: nothing to show yet.
            None => div().into_any_element(),
            // Reconnecting keeps what is known on screen.
            Some(ConnectionState::Connected | ConnectionState::Reconnecting) => {
                match (self.nav.thread, self.nav.channel) {
                    (Some(thread), _) if self.state.thread(thread).is_some() => {
                        self.render_thread(thread, cx).into_any_element()
                    }
                    (_, Some(channel)) if self.nav.drafting => {
                        self.render_new_thread(channel).into_any_element()
                    }
                    (_, Some(channel)) if self.state.channel(channel).is_some() => {
                        self.render_thread_list(channel, cx).into_any_element()
                    }
                    _ => render_nothing_selected().into_any_element(),
                }
            }
            Some(state) => self.render_connect(state, cx).into_any_element(),
        }
    }
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
mod tests {
    use super::{near_bottom, thread_title, working_hint};

    #[test]
    fn a_title_is_the_first_line_cut_to_80() {
        assert_eq!(
            thread_title("Fix the login\nIt fails on Safari"),
            "Fix the login"
        );
        assert_eq!(thread_title("\n  @Forge look  \nmore"), "@Forge look");
        assert_eq!(thread_title(&"x".repeat(100)).chars().count(), 80);
    }

    #[test]
    fn new_messages_scroll_only_from_near_the_bottom() {
        // Offsets are negative as content scrolls up.
        assert!(near_bottom(-500., 500.));
        assert!(near_bottom(-470., 500.), "within 40 px");
        assert!(!near_bottom(-400., 500.), "you scrolled up to read");
        assert!(near_bottom(0., 0.), "nothing to scroll");
    }

    #[test]
    fn the_hint_names_who_is_working() {
        assert_eq!(working_hint(&[]), None);
        assert_eq!(
            working_hint(&["Forge".into()]).as_deref(),
            Some("Forge is working. A reply now is queued for its next turn.")
        );
        assert_eq!(
            working_hint(&["Forge".into(), "Scout".into(), "Prism".into()]).as_deref(),
            Some("Forge, Scout and Prism are working. A reply now is queued for their next turns.")
        );
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::super::test_ui::{click, has, notifications, open, shows, wait_to_show, wait_until};
    use crate::client;
    use crate::connection::ConnectionConfig;
    use crate::test_support::{Daemon, refuse_hello, short_home};
    use crew_core::ids::{ChannelId, ProjectId, ThreadId};
    use crew_proto::Request;
    use gpui_kit::TestAppContext;

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
