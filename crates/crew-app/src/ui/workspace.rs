//! The window's root view: title bar, sidebar, main area and status bar.
//! The views themselves live in their own files; this one holds what is
//! selected and talks to the client.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ApprovalId, ChannelId, ProjectId, RunId, ThreadId};
use crew_proto::{Request, RuntimeInfo};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::SelectState;
use gpui_kit::*;

use super::approval_card::BriefEditor;
use super::channel_view::ChannelView;
use super::common::runtime_chip;
use super::composer::{Composer, ComposerEvent, ComposerMode};
use super::diff_view::{Review, SavedMessage};
use super::first_launch::FirstLaunch;
use super::inspector::Inspector;
use super::markdown_view::DocCache;
use super::mention_menu::MentionAgent;
use super::running_view::RunningView;
use super::service_control::ServiceAction;
use super::settings::projects::{PickFolder, native_picker};
use super::settings::{Page, SettingsView, editor_items};
use super::terminal_view::TerminalView;
use crate::app::{
    ApproveLatest, ArchiveThread, NewThread, OpenSettings, RenameThread, Search, ShowRunning,
    ShowThreads, StopAll,
};
use crate::client::{self, ClientEvent, ClientHandle};
use crate::connection::{ConnectionConfig, ConnectionState};
use crate::fonts::ui_font;
use crate::model::{Handoff, State};
use crate::recents::Recents;
use crate::theme::color;

pub const SIDEBAR_WIDTH: f32 = 264.;
/// Narrower windows hide the inspector until you show it.
pub const INSPECTOR_MIN_WINDOW: f32 = 1200.;
/// Longest thread title taken from a first message.
pub const TITLE_MAX: usize = 80;

/// What is selected and open, kept in memory only.
#[derive(Default)]
pub(super) struct Nav {
    /// Projects whose channels are hidden.
    pub(super) collapsed: HashSet<ProjectId>,
    /// Projects whose "In terminal" sessions are listed.
    pub(super) terminal_groups: HashSet<ProjectId>,
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

/// How often the sidebar redraws terminal sessions' "5m ago".
const TERMINAL_TIMES_EVERY: Duration = Duration::from_secs(30);

/// How often the status bar asks which agent CLIs are ready.
const RUNTIMES_EVERY: Duration = Duration::from_secs(10 * 60);

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
    /// The open terminal session, shown read-only in the main area.
    pub(super) terminal: Option<TerminalView>,
    /// A terminal session to open once it is known (debug builds'
    /// startup hook).
    terminal_pending: Option<String>,
    /// Redraws the open thread's timers once a second while a run works.
    ticker: Option<Task<()>>,
    /// Redraws the sidebar's terminal session times while a project's
    /// "In terminal" list is open.
    terminal_ticker: Option<Task<()>>,
    /// Requests waiting for their answers, by a number of their own.
    requests: HashMap<u64, Task<()>>,
    next_request: u64,
    /// Starting crewd and waiting for it to answer.
    pub(super) service_task: Option<Task<()>>,
    /// The bundled crewd launchd runs, when the app runs from its bundle.
    /// A dev build starts the crewd beside it instead.
    pub(super) bundled_crewd: Option<std::path::PathBuf>,
    /// An out-of-date crewd was restarted, or found busy, once already.
    pub(super) auto_restarted: bool,
    /// The last error shown as a notification.
    pub(super) last_error: Option<SharedString>,
    pub(super) inspector: Inspector,
    /// Agent messages read as Markdown, by message or event.
    pub(super) docs: DocCache,
    /// The handoff brief you are editing, if any.
    pub(super) editing: Option<BriefEditor>,
    /// Permission asks whose whole input is shown.
    pub(super) open_inputs: HashSet<ApprovalId>,
    /// Approvals whose answer was sent.
    pub(super) deciding: HashSet<ApprovalId>,
    /// Whether you showed or hid the inspector. Until you do, it follows
    /// the window's width.
    pub(super) inspector_open: Option<bool>,
    /// Whether the inspector is shown, as last drawn.
    pub(super) inspector_shown: bool,
    /// The open review of a thread's changes, shown in place of the body.
    pub(super) review: Option<Review>,
    /// Review changes shows lines side by side. Kept while the app runs.
    pub(super) diff_split: bool,
    /// Files you marked as viewed, per thread, kept in memory only.
    pub(super) viewed: HashMap<ThreadId, HashSet<String>>,
    /// A thread whose review opens once the thread is open (debug
    /// builds' startup hook).
    review_pending: Option<ThreadId>,
    /// Commit messages of closed reviews, per thread.
    pub(super) commit_messages: HashMap<ThreadId, SavedMessage>,
    /// Numbers the thread diff reads, so only the latest lands.
    pub(super) diff_seq: u64,
    /// Running now, shown in place of the body.
    pub(super) running: Option<RunningView>,
    /// A Stop all request is waiting for its answer.
    pub(super) stopping_all: bool,
    /// The agent CLIs crewd found, as last asked.
    pub(super) runtimes: Vec<RuntimeInfo>,
    /// Asks crewd for the runtimes now and every [`RUNTIMES_EVERY`].
    runtimes_task: Option<Task<()>>,
    /// A Refresh of the runtimes is waiting for its answer.
    pub(super) runtimes_checking: bool,
    pub(super) runtimes_refresh: Option<Task<()>>,
    /// Settings, shown in place of the body.
    pub(super) settings: Option<SettingsView>,
    /// Why the last change of limits failed, shown on their page.
    pub(super) limits_error: Option<String>,
    /// The first launch, shown in place of the body until it is done.
    pub(super) first_launch: Option<FirstLaunch>,
    /// Whether the first snapshot was seen, which decides the first
    /// launch.
    snapshot_seen: bool,
    /// Opens a folder picker.
    pub(super) pick_folder: PickFolder,
    /// Picking a folder and asking git about it.
    pub(super) folder_task: Option<Task<()>>,
    /// The editor you chose in settings, if you did.
    pub(super) editor_choice: Option<String>,
    /// The editors found on this Mac.
    pub(super) editors: Vec<String>,
    /// Launches the editor.
    pub(super) opener: crate::editor::Opener,
    /// The Editor row of General.
    pub(super) editor_select: Entity<EditorSelect>,
    _editor_events: Subscription,
    /// The selected channel's cards and toolbar.
    pub(super) channel_view: ChannelView,
    /// The thread or channel dialog on screen, if any.
    pub(super) dialog: Option<super::dialogs::OpenDialog>,
    /// The ⌘K palette, while it is open.
    pub(super) palette: Option<Entity<super::palette::Palette>>,
    /// What you opened lately, for the palette.
    pub(super) recents: Recents,
    /// Writing the recents to the prefs file.
    recents_task: Option<Task<()>>,
}

/// A place the palette, and the sidebar, can take you to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Place {
    Thread(ThreadId),
    Channel(ChannelId),
    Agent(AgentId),
}

/// The Editor select of General.
pub(super) type EditorSelect = SelectState<Vec<SharedString>>;

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
                    ComposerEvent::Cancel => this.cancel_new_thread(window, cx),
                    ComposerEvent::OpenAgentSettings => {
                        this.open_settings(Page::Agents, window, cx);
                        this.new_agent(window, cx);
                    }
                },
            );
        let editor_choice = crate::editor::load_choice(&conn.crew_home);
        let recents = Recents::load(&conn.crew_home);
        let editors = crate::editor::detect_installed();
        let editor_select = cx.new(|cx| {
            let (labels, ix) = editor_items(editor_choice.as_deref(), &editors);
            SelectState::new(labels, ix, window, cx)
        });
        let editor_events = cx.subscribe_in(&editor_select, window, Self::editor_picked);
        let channel_view = ChannelView::new(window, cx);
        let hooks = dev_hooks();
        let bundled_crewd = std::env::current_exe()
            .ok()
            .and_then(|exe| crate::launchd::bundled_crewd(&exe));
        if bundled_crewd.is_some() {
            // Loads crewd's LaunchAgent, which starts it if needed.
            cx.defer_in(window, |this, window, cx| {
                this.service(ServiceAction::Start, window, cx)
            });
        }
        // Window-wide shortcuts such as Stop all need a focused element
        // inside the workspace, even before anything is clicked.
        let focus = cx.focus_handle();
        let running = hooks.running.then(|| RunningView::new(cx.focus_handle()));
        let settings = hooks
            .settings
            .map(|page| SettingsView::new(page, cx.focus_handle()));
        running
            .as_ref()
            .map(|r| &r.focus)
            .or(settings.as_ref().map(|s| &s.focus))
            .unwrap_or(&focus)
            .focus(window, cx);
        Self {
            conn,
            state: State::default(),
            client,
            _client_events: Self::take_in_events(events, window, cx),
            starting: false,
            composer,
            _composer_events: composer_events,
            focus,
            nav: Nav::default(),
            drafts: HashMap::new(),
            creating: None,
            open_pending: hooks.review.or(hooks.open_thread),
            terminal: None,
            terminal_pending: hooks.terminal,
            ticker: None,
            terminal_ticker: None,
            requests: HashMap::new(),
            next_request: 0,
            service_task: None,
            bundled_crewd,
            auto_restarted: false,
            last_error: None,
            inspector: Inspector::default(),
            docs: DocCache::default(),
            editing: None,
            open_inputs: HashSet::new(),
            deciding: HashSet::new(),
            inspector_open: None,
            inspector_shown: false,
            review: None,
            diff_split: false,
            viewed: HashMap::new(),
            review_pending: hooks.review,
            commit_messages: HashMap::new(),
            diff_seq: 0,
            running,
            stopping_all: false,
            runtimes: Vec::new(),
            runtimes_task: None,
            runtimes_checking: false,
            runtimes_refresh: None,
            settings,
            limits_error: None,
            first_launch: None,
            snapshot_seen: false,
            pick_folder: native_picker(),
            folder_task: None,
            editor_choice,
            editors,
            opener: crate::editor::system_opener(),
            editor_select,
            _editor_events: editor_events,
            channel_view,
            dialog: None,
            palette: None,
            recents,
            recents_task: None,
        }
    }

    /// Replaces the folder picker, as tests do.
    #[cfg(test)]
    pub(super) fn set_folder_picker(&mut self, pick: PickFolder) {
        self.pick_folder = pick;
    }

    /// Moves focus off any field onto the window, as clicking empty space
    /// would.
    #[cfg(test)]
    pub(super) fn focus_window(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    /// What the UI renders from.
    pub fn state(&self) -> &State {
        &self.state
    }

    pub(super) fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    /// Runs the sidebar's 30 s redraw only while an open "In terminal"
    /// list has sessions to show.
    pub(super) fn sync_terminal_ticker(&mut self, cx: &mut Context<Self>) {
        let shown = self.nav.terminal_groups.iter().any(|p| {
            !self.nav.collapsed.contains(p) && !self.state.terminal_sessions(*p).is_empty()
        });
        if !shown {
            self.terminal_ticker = None;
        } else if self.terminal_ticker.is_none() {
            self.terminal_ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TERMINAL_TIMES_EVERY).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// Whether the sidebar's terminal session times are kept fresh.
    #[cfg(test)]
    pub(super) fn terminal_ticking(&self) -> bool {
        self.terminal_ticker.is_some()
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
        if let Some(session) = &self.terminal_pending
            && self.state.terminal_session(session).is_some()
        {
            let session = session.clone();
            self.terminal_pending = None;
            self.open_terminal(session, window, cx);
        }
        if let Some(thread) = self.open_pending
            && self.state.thread(thread).is_some()
        {
            self.open_pending = None;
            self.open_thread(thread, window, cx);
            if self.review_pending == Some(thread) {
                self.review_pending = None;
                self.open_review(thread, window, cx);
            }
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
        let mut connected = false;
        let mut snapshot = false;
        let mut terminal_gone = false;
        let mut run_ended = false;
        let mut touched = Vec::new();
        let mut stale = Vec::new();
        for event in events {
            if let ClientEvent::Msg(msg) = &event {
                touched.extend(touched_thread(msg));
            }
            if let ClientEvent::Msg(crew_proto::ServerMsg::ChannelOverviewStale { channel }) =
                &event
            {
                stale.push(*channel);
            }
            // Only crewd saying so closes the open session: a snapshot
            // just after crewd starts may not list it yet.
            if let ClientEvent::Msg(crew_proto::ServerMsg::TerminalSessionRemoved { id }) = &event
                && self.terminal.as_ref().is_some_and(|t| t.session == *id)
            {
                terminal_gone = true;
            }
            if let ClientEvent::Msg(crew_proto::ServerMsg::RunStatus { status, .. }) = &event {
                run_ended |= !matches!(status, RunStatus::Queued | RunStatus::Working);
            }
            connected |= event == ClientEvent::Connected;
            snapshot |= matches!(
                event,
                ClientEvent::Msg(crew_proto::ServerMsg::Snapshot { .. })
            );
            if let ClientEvent::Msg(crew_proto::ServerMsg::RuntimesChanged { runtimes }) = &event {
                self.runtimes = runtimes.clone();
            }
            event.apply_to(&mut self.state);
        }
        if connected {
            self.watch_runtimes(cx);
        }
        if matches!(
            self.state.connection(),
            Some(ConnectionState::VersionMismatch { .. })
        ) && self.bundled_crewd.is_some()
            && !self.starting
            && !std::mem::replace(&mut self.auto_restarted, true)
        {
            self.service(ServiceAction::AutoRestart, window, cx);
        }
        // Only the first snapshot decides: an empty install starts with
        // the first launch.
        if snapshot && !std::mem::replace(&mut self.snapshot_seen, true) {
            if self.state.projects().is_empty() {
                self.first_launch = Some(FirstLaunch::default());
            }
            self.page_opened(window, cx);
        }
        // Today's runs change when one ends, and may have while away.
        if run_ended || snapshot {
            self.refresh_usage(cx);
            self.refresh_history(cx);
        }
        self.channel_touched(&touched, &stale, snapshot);
        self.sync_composer_mode(window, cx);
        self.sync_settings(window, cx);
        self.sync(cx);
        self.sync_inspector(window, cx);
        self.sync_review_draft(window, cx);
        self.sync_review_comments(cx);
        self.open_if_known(window, cx);
        let open = self.terminal.as_ref().map(|t| t.session.as_str());
        if terminal_gone && open.is_some_and(|id| self.state.terminal_session(id).is_none()) {
            self.close_terminal();
        }
        self.sync_terminal();
        self.sync_terminal_ticker(cx);
        cx.notify();
    }

    /// The name of the agent that ran last in the open thread, if it still
    /// exists. A reply that mentions no one goes to it.
    fn last_agent(&self) -> Option<String> {
        let thread = self.nav.thread?;
        let agents = self.state.agents();
        self.state
            .thread_runs(thread)
            .into_iter()
            .rev()
            .find_map(|r| agents.iter().find(|a| a.id == r.agent))
            .map(|a| a.spec.name.clone())
    }

    /// Brings the composer, the timer and the scroll position in line
    /// with what is known and open.
    pub(super) fn sync(&mut self, cx: &mut Context<Self>) {
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
                runtime: a.spec.runtime.clone(),
                status: self.state.agent_status(a.id),
            })
            .collect();
        let live = self
            .nav
            .thread
            .map(|t| self.state.live_runs(t))
            .unwrap_or_default();
        let working: Vec<String> = live
            .iter()
            .filter(|r| r.status == RunStatus::Working)
            .filter_map(|r| self.state.agent(r.agent).map(|a| a.spec.name.clone()))
            .collect();
        let hint = if live.is_empty() {
            self.last_agent().map(|name| reply_hint(&name))
        } else {
            working_hint(&working)
        };
        let busy = self.creating.is_some()
            && matches!(self.draft_key(), Some(DraftKey::NewThread(c)) if self.creating == Some(c));
        let base = self
            .nav
            .channel
            .and_then(|c| self.state.channel(c))
            .and_then(|c| self.state.project(c.project))
            .map(|p| p.main_branch.clone())
            .unwrap_or_default();
        self.composer.update(cx, |composer, cx| {
            composer.set_context(agents, project, hint, cx);
            composer.set_busy(busy, cx);
            composer.set_base(base, cx);
        });

        let running_ticks = self.running.is_some()
            && self
                .state
                .running()
                .iter()
                .any(|r| r.status == RunStatus::Working);
        if working.is_empty() && !running_ticks && !self.channel_ticks() {
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

        self.sync_channel(cx);

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
        self.close_thread(cx);
        self.nav.drafting = false;
        self.look_at(Some(channel));
        if self.recents.visited_channel(channel) {
            self.save_recents(cx);
        }
        self.reset_channel_view(channel, window, cx);
        self.sync_composer_mode(window, cx);
        if self.composer_mode() != ComposerMode::Empty {
            // The composer isn't shown, so its shortcuts go to the view.
            self.focus.focus(window, cx);
        }
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
            self.close_thread(cx);
            self.watch_thread(thread);
            self.nav.thread = Some(thread);
            self.channel_view.selected = Some(thread);
            self.nav.seen_messages = 0;
            self.nav.stick_to_bottom = true;
            self.sync_inspector(window, cx);
        }
        self.nav.drafting = false;
        let channel = self.state.thread(thread).map(|t| t.channel);
        self.look_at(channel.or(self.nav.channel));
        if self.recents.opened_thread(thread) {
            self.save_recents(cx);
        }
        self.sync_composer_mode(window, cx);
        self.composer
            .update(cx, |composer, cx| composer.focus(window, cx));
        self.load_draft(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// Takes you to `place`, leaving settings, Running now or a review.
    /// Every way to a thread, channel or agent from the palette goes
    /// through here. An agent opens its settings for now.
    pub(super) fn open_place(&mut self, place: Place, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_pages(cx);
        match place {
            Place::Thread(thread) => self.open_thread(thread, window, cx),
            Place::Channel(channel) => self.select_channel(channel, window, cx),
            Place::Agent(agent) => {
                if self.recents.visited_agent(agent) {
                    self.save_recents(cx);
                }
                self.open_settings(Page::Agents, window, cx);
                self.edit_agent(agent, window, cx);
            }
        }
    }

    /// Closes settings, Running now and the review, which show in place
    /// of the body.
    pub(super) fn leave_pages(&mut self, cx: &mut Context<Self>) {
        self.settings = None;
        self.running = None;
        self.drop_review(cx);
        cx.notify();
    }

    /// Writes the recents to the prefs file, off the UI thread, after any
    /// write still pending, so none is dropped and the latest lands last.
    fn save_recents(&mut self, cx: &mut Context<Self>) {
        let recents = self.recents.clone();
        let home = self.conn.crew_home.clone();
        let pending = self.recents_task.take();
        self.recents_task = Some(cx.background_executor().spawn(async move {
            if let Some(pending) = pending {
                pending.await;
            }
            if let Err(e) = recents.save(&home) {
                eprintln!("crew: couldn't save recent places: {e}");
            }
        }));
    }

    /// Starts a new thread in the selected channel: the composer takes its
    /// first message.
    pub(super) fn new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.nav.channel.is_none() {
            return;
        }
        self.stash_draft(cx);
        self.close_thread(cx);
        self.nav.drafting = true;
        self.sync_composer_mode(window, cx);
        self.composer
            .update(cx, |composer, cx| composer.focus(window, cx));
        self.load_draft(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// Closes the new thread composer, dropping what you wrote in it.
    pub(super) fn cancel_new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.nav.drafting || self.creating.is_some() {
            return;
        }
        if let Some(channel) = self.nav.channel {
            self.drafts.remove(&DraftKey::NewThread(channel));
        }
        self.composer
            .update(cx, |composer, cx| composer.set_text("", window, cx));
        self.nav.drafting = false;
        self.sync_composer_mode(window, cx);
        self.focus.focus(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// What the composer is for, given what is open: a reply, a new
    /// thread, or the first thread of an empty channel.
    pub(super) fn composer_mode(&self) -> ComposerMode {
        match (self.nav.thread, self.nav.channel) {
            (Some(_), _) => ComposerMode::Reply,
            (None, Some(channel)) if self.channel_is_empty(channel) => ComposerMode::Empty,
            (None, Some(_)) if self.nav.drafting => ComposerMode::NewThread,
            _ => ComposerMode::Reply,
        }
    }

    /// Whether `channel` is known and has no threads, archived or not.
    pub(super) fn channel_is_empty(&self, channel: ChannelId) -> bool {
        self.state.channel(channel).is_some()
            && self.state.threads_with_archived(channel).is_empty()
    }

    /// Puts the composer in the mode for what is open. A first message
    /// you were writing in an empty channel that gets a thread meanwhile
    /// stays, in the new thread composer.
    fn sync_composer_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was = self.composer.read(cx).mode();
        let mut mode = self.composer_mode();
        if was == ComposerMode::Empty
            && mode == ComposerMode::Reply
            && self.nav.thread.is_none()
            && self.terminal.is_none()
            && !self.composer.read(cx).text(cx).trim().is_empty()
        {
            self.nav.drafting = true;
            mode = ComposerMode::NewThread;
        }
        self.composer
            .update(cx, |composer, cx| composer.set_mode(mode, window, cx));
    }

    /// Whose text the composer holds now.
    fn draft_key(&self) -> Option<DraftKey> {
        match (self.nav.thread, self.nav.channel) {
            (Some(thread), _) => Some(DraftKey::Thread(thread)),
            (None, Some(channel))
                if self.nav.drafting
                    || (self.terminal.is_none() && self.channel_is_empty(channel)) =>
            {
                Some(DraftKey::NewThread(channel))
            }
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

    /// Leaves the open thread, draft or channel for a terminal session.
    pub(super) fn leave_for_terminal(&mut self, cx: &mut Context<Self>) {
        self.stash_draft(cx);
        self.close_thread(cx);
        self.nav.drafting = false;
        self.look_at(None);
        self.sync(cx);
    }

    fn close_thread(&mut self, cx: &App) {
        self.close_terminal();
        self.drop_review(cx);
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
                // The thread may land before its answer: stay on its draft.
                self.nav.drafting = true;
                self.sync_composer_mode(window, cx);
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

    /// Stops every run in every project. Sent once until crewd answers.
    pub(super) fn stop_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stopping_all {
            return;
        }
        self.stopping_all = true;
        let answer = self.client.request(Request::StopAll);
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await.map(drop);
            this.update(cx, |this, cx| {
                this.stopping_all = false;
                cx.notify();
            })
            .ok();
            result.map_err(|e| (e, None))
        });
        cx.notify();
    }

    /// Asks crewd which agent CLIs are ready, now and every
    /// [`RUNTIMES_EVERY`] after. A new connection starts over.
    fn watch_runtimes(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        self.runtimes_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let answer = client.request(Request::Runtimes).await;
                let runtimes = answer.and_then(|v| {
                    serde_json::from_value::<Vec<RuntimeInfo>>(v).map_err(|e| e.to_string())
                });
                // A failed ask keeps what was known; the next one may work.
                match runtimes {
                    Ok(runtimes) => {
                        let kept = this.update(cx, |this, cx| {
                            this.runtimes = merge_runtimes(&this.runtimes, runtimes);
                            cx.notify();
                        });
                        if kept.is_err() {
                            break;
                        }
                    }
                    Err(e) => eprintln!("crew: couldn't ask which agent CLIs are ready: {e}"),
                }
                cx.background_executor().timer(RUNTIMES_EVERY).await;
            }
        }));
    }

    /// Gives the workspace itself focus, so its shortcuts work.
    pub(super) fn focus_workspace(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    /// Sends `req`, showing its error if it fails.
    pub(super) fn request(&mut self, req: Request, window: &mut Window, cx: &mut Context<Self>) {
        let answer = self.client.request(req);
        self.spawn_request(window, cx, async move |_, _| {
            answer.await.map(drop).map_err(|e| (e, None))
        });
    }

    /// Sends the channel's whole handoff setting. It shows at once and
    /// stays until crewd reports it, so quick clicks build on each other.
    pub(super) fn send_channel_handoff(
        &mut self,
        channel: ChannelId,
        (policy, budget): Handoff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.expect_channel_handoff(channel, (policy, budget));
        let answer = self.client.request(Request::SetChannelHandoff {
            channel,
            policy,
            budget,
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await.map(drop);
            if result.is_err() {
                this.update(cx, |this, cx| {
                    this.state.drop_channel_handoff(channel);
                    cx.notify();
                })
                .ok();
            }
            result.map_err(|e| (e, None))
        });
        cx.notify();
    }

    /// Sends the thread's whole handoff setting, as for channels.
    pub(super) fn send_thread_handoff(
        &mut self,
        thread: ThreadId,
        (policy, budget): Handoff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.expect_thread_handoff(thread, (policy, budget));
        let answer = self.client.request(Request::SetThreadHandoff {
            thread,
            policy,
            budget,
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await.map(drop);
            if result.is_err() {
                this.update(cx, |this, cx| {
                    this.state.drop_thread_handoff(thread);
                    cx.notify();
                })
                .ok();
            }
            result.map_err(|e| (e, None))
        });
        cx.notify();
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

/// The thread a message from crewd is about, if any.
fn touched_thread(msg: &crew_proto::ServerMsg) -> Option<ThreadId> {
    use crew_proto::ServerMsg;
    match msg {
        ServerMsg::Message { thread, .. }
        | ServerMsg::RunStatus { thread, .. }
        | ServerMsg::Approval { thread, .. } => Some(*thread),
        ServerMsg::ThreadCreated { thread, .. } | ServerMsg::ThreadChanged { thread } => {
            Some(thread.id)
        }
        _ => None,
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

/// What to open at startup: a thread, a review or Running now.
#[derive(Default)]
struct StartupHooks {
    open_thread: Option<ThreadId>,
    review: Option<ThreadId>,
    running: bool,
    settings: Option<Page>,
    terminal: Option<String>,
}

/// `CREW_APP_OPEN_THREAD` and
/// `CREW_APP_SHOW=diff:<thread>|running|terminal:<session>`, in debug
/// builds only.
fn dev_hooks() -> StartupHooks {
    #[cfg(debug_assertions)]
    {
        let hooks = super::dev_hooks::read(|k| std::env::var(k).ok());
        StartupHooks {
            open_thread: hooks.open_thread,
            review: hooks.review,
            running: hooks.running,
            settings: hooks.settings,
            terminal: hooks.terminal,
        }
    }
    #[cfg(not(debug_assertions))]
    {
        StartupHooks::default()
    }
}

/// "Replying to Forge. Type @ to bring in someone else."
fn reply_hint(name: &str) -> String {
    format!("Replying to {name}. Type @ to bring in someone else.")
}

/// "Forge is working. Replies without @Forge become context for its next
/// turn."
fn working_hint(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!(
            "{one} is working. Replies without @{one} become context for its next turn."
        )),
        [rest @ .., last] => Some(format!(
            "{} and {last} are working. Replies without an @mention become context for their next turns.",
            rest.join(", ")
        )),
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let wide = window.viewport_size().width >= px(INSPECTOR_MIN_WINDOW);
        self.inspector_shown = self.inspector_open.unwrap_or(wide);
        self.refresh_channel_rows(cx);
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NewThread, window, cx| this.new_thread(window, cx)))
            .on_action(
                cx.listener(|this, _: &ApproveLatest, window, cx| this.approve_latest(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RenameThread, window, cx| this.rename_selected(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ArchiveThread, window, cx| {
                    this.archive_selected(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &StopAll, window, cx| this.stop_all(window, cx)))
            .on_action(
                cx.listener(|this, _: &ShowRunning, window, cx| this.show_running(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShowThreads, window, cx| this.show_threads(window, cx)),
            )
            .on_action(cx.listener(|this, _: &Search, window, cx| this.open_palette(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.open_settings(Page::General, window, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(color("bg-100"))
            .text_color(color("text-primary"))
            .font_family(ui_font())
            .text_size(px(13.))
            .child(self.render_title_bar(cx))
            .child(if self.settings.is_some() {
                self.render_settings(cx)
            } else if self.running.is_some() {
                self.render_running(cx)
            } else if self.review.is_some() {
                self.render_review(cx)
            } else if self.first_launch.is_some() && self.inspector_connected() {
                self.render_first_launch(cx)
            } else {
                self.render_body(cx)
            })
            .child(self.render_status_bar(cx))
    }
}

impl Workspace {
    /// The sidebar, the main area and the inspector.
    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
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
                    .filter(|_| self.inspector_shown)
                    .map(|t| self.render_inspector(t, cx)),
            )
            .into_any_element()
    }
}

impl Workspace {
    /// Shows the inspector if it is hidden, or hides it.
    pub(super) fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector_open = Some(!self.inspector_shown);
        cx.notify();
    }

    /// Connected, or reconnecting with what was known still shown.
    fn inspector_connected(&self) -> bool {
        matches!(
            self.state.connection(),
            Some(ConnectionState::Connected | ConnectionState::Reconnecting)
        )
    }

    /// Leaves settings and Running now for the threads.
    fn show_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_settings(window, cx);
        self.close_running(window, cx);
    }

    /// The thread the inspector shows: the open one, while connected.
    fn inspector_thread(&self) -> Option<ThreadId> {
        let shown = self.inspector_connected();
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
                if self.terminal.is_some() {
                    return self.render_terminal(cx);
                }
                match (self.nav.thread, self.nav.channel) {
                    (Some(thread), _) if self.state.thread(thread).is_some() => {
                        self.render_thread(thread, cx).into_any_element()
                    }
                    (_, Some(channel)) if self.state.channel(channel).is_some() => {
                        self.render_channel_view(channel, cx)
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

/// The runtimes a `Runtimes` answer brings, keeping a known version and
/// sign-in where the answer has neither for an installed CLI. A slow check
/// answers with only what it found, and its RuntimesChanged may land first.
fn merge_runtimes(known: &[RuntimeInfo], answer: Vec<RuntimeInfo>) -> Vec<RuntimeInfo> {
    answer
        .into_iter()
        .map(|new| {
            let bare = new.installed && new.version.is_none() && new.signed_in.is_none();
            let fuller = known.iter().find(|k| {
                k.runtime == new.runtime
                    && k.installed
                    && (k.version.is_some() || k.signed_in.is_some())
            });
            match fuller {
                Some(k) if bare => k.clone(),
                _ => new,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{merge_runtimes, near_bottom, reply_hint, thread_title, working_hint};
    use crew_core::agent::Runtime;
    use crew_proto::RuntimeInfo;

    fn info(runtime: Runtime, version: Option<&str>, signed_in: Option<bool>) -> RuntimeInfo {
        RuntimeInfo {
            runtime,
            installed: true,
            version: version.map(Into::into),
            signed_in,
            path: Some("/usr/local/bin/x".into()),
            models: vec![],
        }
    }

    /// A quick answer that only says what was found doesn't overwrite the
    /// fuller one a RuntimesChanged already brought.
    #[test]
    fn a_bare_runtimes_answer_keeps_what_is_known() {
        let known = vec![
            info(Runtime::ClaudeCode, Some("2.1.0"), Some(true)),
            info(Runtime::Codex, None, None),
        ];
        let bare = vec![
            info(Runtime::ClaudeCode, None, None),
            info(Runtime::Codex, Some("0.5.0"), None),
        ];
        assert_eq!(
            merge_runtimes(&known, bare),
            vec![
                info(Runtime::ClaudeCode, Some("2.1.0"), Some(true)),
                info(Runtime::Codex, Some("0.5.0"), None),
            ]
        );
        // Not installed any more wins.
        let gone = RuntimeInfo {
            installed: false,
            path: None,
            ..info(Runtime::ClaudeCode, None, None)
        };
        assert_eq!(merge_runtimes(&known, vec![gone.clone()]), vec![gone]);
    }

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
    fn the_hint_names_who_a_reply_goes_to() {
        assert_eq!(
            reply_hint("Forge"),
            "Replying to Forge. Type @ to bring in someone else."
        );
    }

    #[test]
    fn the_hint_names_who_is_working() {
        assert_eq!(working_hint(&[]), None);
        assert_eq!(
            working_hint(&["Forge".into()]).as_deref(),
            Some("Forge is working. Replies without @Forge become context for its next turn.")
        );
        assert_eq!(
            working_hint(&["Forge".into(), "Scout".into(), "Prism".into()]).as_deref(),
            Some(
                "Forge, Scout and Prism are working. Replies without an @mention become context for their next turns."
            )
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
    use gpui_kit::{Entity, TestAppContext};

    use super::Workspace;
    use crate::connection::ConnectionState;

    fn wait_connected(cx: &mut TestAppContext, ws: &Entity<Workspace>) {
        wait_until(cx, "the connection", |cx| {
            ws.read_with(cx, |ws, _| {
                ws.state().connection() == Some(&ConnectionState::Connected)
            })
        });
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

    /// A project made by another client, so the first launch is skipped.
    fn one_project(home: &std::path::Path) {
        let (other, _events) = client::spawn(ConnectionConfig {
            crew_home: home.to_path_buf(),
        });
        futures::executor::block_on(other.request(Request::CreateProject {
            name: "demo".into(),
            root: "/work/demo".into(),
            main_branch: "main".into(),
        }))
        .unwrap();
    }

    fn wait_for_projects(cx: &mut TestAppContext, ws: &Entity<Workspace>) {
        wait_until(cx, "the snapshot", |cx| {
            ws.read_with(cx, |ws, _| !ws.state().projects().is_empty())
        });
    }

    #[gpui_kit::test]
    fn connected_daemon_shows_the_workspace(cx: &mut TestAppContext) {
        let home = short_home();
        let _daemon = Daemon::start(home.path().to_path_buf());
        one_project(home.path());
        let (window, ws) = open(cx, home.path());
        wait_connected(cx, &ws);
        wait_for_projects(cx, &ws);
        assert!(shows(cx, window, "Nothing selected"));
        assert!(!shows(cx, window, "Welcome to Crew"), "not a first launch");
        assert!(!has(cx, window, "connect-start"));
        assert!(
            !has(cx, window, "status-connection"),
            "connected says nothing"
        );
    }

    /// crewd goes away and something on its socket never answers: the
    /// client keeps trying, and the workspace stays up meanwhile.
    #[gpui_kit::test]
    fn a_lost_connection_keeps_the_workspace_while_reconnecting(cx: &mut TestAppContext) {
        let home = short_home();
        let daemon = Daemon::start(home.path().to_path_buf());
        one_project(home.path());
        let (window, ws) = open(cx, home.path());
        wait_connected(cx, &ws);
        wait_for_projects(cx, &ws);

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
        let (_, ws) = open(cx, home.path());
        wait_connected(cx, &ws);

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
