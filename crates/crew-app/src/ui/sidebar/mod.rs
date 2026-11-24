//! The sidebar, in one of two forms you switch between. Projects: a
//! filter bar, the projects grouped into Pinned, Active and Other
//! projects with their channels and terminal sessions, then the agents.
//! Threads: every project's threads in one list (see [`threads`]). Rows
//! sit in virtual lists, so many projects and threads stay smooth.

mod filter;
mod projects;
mod summaries;
mod switcher;
mod thread_popover;
pub(crate) mod threads;
mod threads_footer;

use std::collections::HashMap;
use std::path::Path;

use crew_core::ids::{AgentId, ChannelId, ProjectId};
use crew_core::store::StoredAgent;
use crew_proto::TerminalSession;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::InputState;
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{ago, dot, monogram, now_ms, status_word, with_font_probe};
use super::palette::{highlighted, match_range};

use super::workspace::Workspace;
use crate::model::ThreadStatus;
use crate::prefs::{SidebarMode, SidebarPrefs};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};
pub(crate) use filter::bindings;
pub(crate) use filter::{Hit, filter_hits};
use projects::{ProjectInfo, ProjectLook, duplicate_hints, group_projects, sort_projects};

/// The pin before "Pinned", from the board.
const PIN_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M6 1.5h4l-.5 4.5 2.5 2.5H4L6.5 6z"/><path d="M8 8.5v6" stroke-linecap="round"/></svg>"#;

/// What a channel row shows on its right: a status, unread messages or
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelBadge {
    Status(ThreadStatus),
    Unread(usize),
    None,
}

impl ChannelBadge {
    pub fn label(&self) -> String {
        match self {
            Self::Status(status) => status_word(*status).0.to_string(),
            Self::Unread(n) => format!("{n} unread"),
            Self::None => String::new(),
        }
    }
}

/// A group heading in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Heading {
    Pinned,
    Active,
    /// Filter results: "Projects · {n}".
    Projects(usize),
    /// Filter results: "Channels · {n}".
    Channels(usize),
}

impl Heading {
    fn text(self) -> String {
        match self {
            Self::Pinned => "Pinned".into(),
            Self::Active => "Active".into(),
            Self::Projects(n) => format!("Projects · {n}"),
            Self::Channels(n) => format!("Channels · {n}"),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Active => "active",
            Self::Projects(_) => "projects",
            Self::Channels(_) => "channels",
        }
    }
}

/// One row of the sidebar's list.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Row {
    Heading(Heading),
    Project(ProjectLook),
    Channel(ChannelId),
    TerminalGroup(ProjectId),
    TerminalSession(String),
    /// "Other projects · {n}", open or closed.
    Other {
        count: usize,
        open: bool,
    },
    /// A channel the filter found, with its result number.
    ChannelHit {
        channel: ChannelId,
        hit: usize,
    },
    NoMatches,
    Divider,
    AgentsHeading,
    Agent(AgentId),
}

/// What the rows depend on: they are built again only when it changes.
#[derive(Debug, Clone, PartialEq)]
struct SidebarKey {
    revision: u64,
    collapsed: Vec<ProjectId>,
    terminal_groups: Vec<ProjectId>,
    channel: Option<ChannelId>,
    query: String,
    prefs: SidebarPrefs,
    other_open: bool,
    agents_open: bool,
    /// Projects go quiet as time passes.
    minute: i64,
}

fn sorted_ids(ids: &std::collections::HashSet<ProjectId>) -> Vec<ProjectId> {
    let mut ids: Vec<ProjectId> = ids.iter().copied().collect();
    ids.sort_by_key(|p| p.0);
    ids
}

/// What grouping needs to know about `project`.
fn project_info(
    project: &crew_core::store::StoredProject,
    facts: Option<&crate::model::ProjectFacts>,
) -> ProjectInfo {
    ProjectInfo {
        id: project.id,
        name: project.name.clone(),
        pinned: project.pinned_at.is_some(),
        live: facts.is_some_and(|f| f.status != ThreadStatus::Idle),
        activity: facts.and_then(|f| f.activity),
        created_at: project.created_at,
    }
}

/// The sidebar's own state, kept in memory except the filter menu's
/// choices, which live in the app prefs.
pub(super) struct Sidebar {
    pub(super) filter: Entity<InputState>,
    /// What the filter field holds.
    pub(super) query: String,
    /// The highlighted filter result.
    pub(super) selected: usize,
    pub(super) menu_open: bool,
    /// Where the press that closed the menu was, so the button under it
    /// doesn't open it again.
    pub(super) menu_closed_at: Option<Point<Pixels>>,
    pub(super) menu_focus: FocusHandle,
    pub(super) prefs: SidebarPrefs,
    /// Writing the prefs.
    prefs_task: Option<Task<()>>,
    /// Other projects are listed.
    pub(super) other_open: bool,
    /// The agents are listed under their heading.
    pub(super) agents_open: bool,
    pub(super) list: ListState,
    pub(super) rows: Vec<Row>,
    /// The filter results, in list order.
    pub(super) hits: Vec<Hit>,
    /// Hints for projects whose names clash.
    pub(super) hints: HashMap<ProjectId, String>,
    /// What the rows were last built from.
    built: Option<SidebarKey>,
    /// The open channel the list last made visible.
    revealed: Option<ChannelId>,
    /// The list scrolls to the open channel once its rows are built.
    scroll_pending: bool,
    /// The form shown when last drawn.
    mode: Option<SidebarMode>,
    /// The pointer is on the switch: its tip shows.
    pub(super) switch_hovered: bool,
    /// The Threads form.
    pub(crate) threads: threads::ThreadsSidebar,
    /// The time grouping uses in place of now, as tests set it.
    #[cfg(test)]
    pub(super) now: Option<i64>,
    _filter_events: Subscription,
}

impl Sidebar {
    pub(super) fn new(crew_home: &Path, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter projects"));
        let events = cx.subscribe_in(&filter, window, Workspace::filter_changed);
        Self {
            filter,
            query: String::new(),
            selected: 0,
            menu_open: false,
            menu_closed_at: None,
            menu_focus: cx.focus_handle(),
            prefs: SidebarPrefs::load(crew_home),
            prefs_task: None,
            other_open: false,
            agents_open: true,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            rows: Vec::new(),
            hits: Vec::new(),
            hints: HashMap::new(),
            built: None,
            revealed: None,
            scroll_pending: false,
            mode: None,
            switch_hovered: false,
            threads: threads::ThreadsSidebar::new(window, cx),
            #[cfg(test)]
            now: None,
            _filter_events: events,
        }
    }
}

impl Workspace {
    /// Builds the rows of the form shown. Switching forms shows the open
    /// thread or channel in the new one.
    pub(super) fn refresh_sidebars(&mut self, cx: &mut Context<Self>) {
        let mode = threads::sidebar_mode(cx);
        if self.sidebar.mode.replace(mode) != Some(mode) {
            self.sidebar.menu_open = false;
            self.sidebar.threads.switcher.open = false;
            self.sidebar.threads.hovered = None;
            self.sidebar.revealed = None;
            self.sidebar.built = None;
            self.sidebar.threads.shown_again();
        }
        match mode {
            SidebarMode::Projects => self.refresh_sidebar(),
            SidebarMode::Threads => {
                self.refresh_threads_sidebar();
                self.sync_thread_summaries(cx);
            }
        }
    }

    /// Builds the list's rows from what is known, keeping the scroll
    /// position when they change.
    pub(super) fn refresh_sidebar(&mut self) {
        let now = self.sidebar_now();
        self.reveal_open_channel(now);
        let key = SidebarKey {
            revision: self.state().revision(),
            collapsed: sorted_ids(&self.nav.collapsed),
            terminal_groups: sorted_ids(&self.nav.terminal_groups),
            channel: self.nav.channel,
            query: self.sidebar.query.clone(),
            prefs: self.sidebar.prefs,
            other_open: self.sidebar.other_open,
            agents_open: self.sidebar.agents_open,
            minute: now / 60_000,
        };
        if self.sidebar.built.as_ref() == Some(&key) {
            return;
        }
        let (rows, hits) = self.sidebar_rows(now);
        let state = self.state();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let named: Vec<(ProjectId, &str, &Path)> = state
            .projects()
            .into_iter()
            .map(|p| (p.id, p.name.as_str(), p.root.as_path()))
            .collect();
        let hints = duplicate_hints(&named, home.as_deref());
        let sidebar = &mut self.sidebar;
        sidebar.built = Some(key);
        sidebar.hints = hints;
        sidebar.hits = hits;
        if sidebar.selected >= sidebar.hits.len() {
            sidebar.selected = 0;
        }
        if rows != sidebar.rows {
            let top = sidebar.list.logical_scroll_top();
            sidebar.list.reset(rows.len());
            sidebar.list.scroll_to(ListOffset {
                item_ix: top.item_ix.min(rows.len().saturating_sub(1)),
                offset_in_item: top.offset_in_item,
            });
            sidebar.rows = rows;
        }
        if std::mem::take(&mut self.sidebar.scroll_pending) {
            self.scroll_to_open_channel();
        }
    }

    /// Scrolls the list so the open channel's row, or its project's,
    /// shows.
    fn scroll_to_open_channel(&mut self) {
        let Some(channel) = self.nav.channel else {
            return;
        };
        let project = self.state().channel(channel).map(|c| c.project);
        let rows = &self.sidebar.rows;
        let row = rows
            .iter()
            .position(|r| *r == Row::Channel(channel))
            .or_else(|| {
                rows.iter()
                    .position(|r| matches!(r, Row::Project(look) if Some(look.project) == project))
            });
        if let Some(row) = row {
            self.sidebar.list.scroll_to_reveal_item(row);
        }
    }

    /// Now, or the time a test set.
    fn sidebar_now(&self) -> i64 {
        #[cfg(test)]
        if let Some(now) = self.sidebar.now {
            return now;
        }
        now_ms()
    }

    /// A channel just opened shows in the list: its project opens, and
    /// so does Other projects if it is there. Shown by Show or not, its
    /// project stays listed (see [`group_projects`]).
    fn reveal_open_channel(&mut self, now: i64) {
        if self.nav.channel == self.sidebar.revealed {
            return;
        }
        self.sidebar.revealed = self.nav.channel;
        self.sidebar.scroll_pending = self.nav.channel.is_some();
        let state = self.state();
        let Some(project) = self
            .nav
            .channel
            .and_then(|c| state.channel(c))
            .and_then(|c| state.project(c.project))
        else {
            return;
        };
        let id = project.id;
        let facts = state.project_facts().get(&id).copied();
        let info = project_info(project, facts.as_ref());
        let in_other = !info.pinned && !info.is_active(now);
        self.nav.collapsed.remove(&id);
        if in_other {
            self.sidebar.other_open = true;
        }
    }

    /// The rows to show, and the filter results among them.
    fn sidebar_rows(&self, now: i64) -> (Vec<Row>, Vec<Hit>) {
        let state = self.state();
        let facts = state.project_facts();
        let infos: Vec<ProjectInfo> = state
            .projects()
            .into_iter()
            .map(|p| project_info(p, facts.get(&p.id)))
            .collect();
        let prefs = self.sidebar.prefs;
        let mut rows = Vec::new();
        let query = self.sidebar.query.trim();
        if !query.is_empty() {
            let mut sorted = infos;
            sort_projects(&mut sorted, prefs.sort);
            let names: Vec<(ProjectId, &str)> =
                sorted.iter().map(|p| (p.id, p.name.as_str())).collect();
            let channels: Vec<(ChannelId, &str)> = sorted
                .iter()
                .flat_map(|p| state.channels(p.id))
                .map(|c| (c.id, c.name.as_str()))
                .collect();
            let (projects, channels) = filter_hits(query, &names, &channels);
            let mut hits = Vec::new();
            if projects.is_empty() && channels.is_empty() {
                rows.push(Row::NoMatches);
            }
            if !projects.is_empty() {
                rows.push(Row::Heading(Heading::Projects(projects.len())));
                for project in projects {
                    let hit = hits.len();
                    hits.push(Hit::Project(project));
                    self.push_project(&mut rows, project, Some(hit));
                }
            }
            if !channels.is_empty() {
                rows.push(Row::Heading(Heading::Channels(channels.len())));
                for channel in channels {
                    let hit = hits.len();
                    hits.push(Hit::Channel(channel));
                    rows.push(Row::ChannelHit { channel, hit });
                }
            }
            return (rows, hits);
        }
        let keep = self
            .nav
            .channel
            .and_then(|c| state.channel(c))
            .map(|c| c.project);
        let groups = group_projects(infos, prefs.show, prefs.sort, now, keep);
        if !groups.pinned.is_empty() {
            rows.push(Row::Heading(Heading::Pinned));
            for project in groups.pinned {
                self.push_project(&mut rows, project, None);
            }
        }
        if !groups.active.is_empty() {
            rows.push(Row::Heading(Heading::Active));
            for project in groups.active {
                self.push_project(&mut rows, project, None);
            }
        }
        if !groups.other.is_empty() {
            let open = self.sidebar.other_open;
            rows.push(Row::Other {
                count: groups.other.len(),
                open,
            });
            if open {
                for project in groups.other {
                    self.push_project(&mut rows, project, None);
                }
            }
        }
        if prefs.agents {
            rows.push(Row::Divider);
            rows.push(Row::AgentsHeading);
            if self.sidebar.agents_open {
                rows.extend(state.agents().into_iter().map(|a| Row::Agent(a.id)));
            }
        }
        (rows, Vec::new())
    }

    /// A project's row and, while it is open, its channels and terminal
    /// sessions. A project with one channel and no sessions is one row.
    fn push_project(&self, rows: &mut Vec<Row>, project: ProjectId, hit: Option<usize>) {
        let state = self.state();
        let channels = state.channels(project);
        let sessions = state.terminal_sessions(project);
        let single = match channels.as_slice() {
            [only] if sessions.is_empty() => Some(only.id),
            _ => None,
        };
        let open = single.is_none() && !self.nav.collapsed.contains(&project);
        rows.push(Row::Project(ProjectLook {
            project,
            single,
            open,
            hit,
        }));
        if !open {
            return;
        }
        rows.extend(channels.into_iter().map(|c| Row::Channel(c.id)));
        if !sessions.is_empty() {
            rows.push(Row::TerminalGroup(project));
            if self.nav.terminal_groups.contains(&project) {
                rows.extend(
                    sessions
                        .into_iter()
                        .map(|s| Row::TerminalSession(s.id.clone())),
                );
            }
        }
    }

    /// The sidebar in the form chosen.
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        match threads::sidebar_mode(cx) {
            SidebarMode::Projects => self.render_projects_sidebar(cx).into_any_element(),
            SidebarMode::Threads => self.render_threads_sidebar(cx).into_any_element(),
        }
    }

    fn render_projects_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let running = self.state().working_runs().len();
        let filtering = !self.sidebar.query.trim().is_empty();
        div()
            .id("sidebar")
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .w(self.sidebar_width())
            .h_full()
            .border_r_1()
            .border_color(color("border-100"))
            .child(self.render_sidebar_bar(cx))
            .child(
                div()
                    .id("sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .when(self.sidebar.menu_open, |list| list.opacity(0.4))
                    .child(
                        list(
                            self.sidebar.list.clone(),
                            cx.processor(|this, ix, _, cx| this.render_sidebar_row(ix, cx)),
                        )
                        .size_full(),
                    )
                    .test_support(),
            )
            .when(filtering, |sidebar| sidebar.child(filter::render_hints()))
            .when(running > 0 && !filtering, |sidebar| {
                sidebar.child(render_footer(running, self.stopping_all, cx))
            })
            .when(self.sidebar.menu_open, |sidebar| {
                sidebar.child(deferred(self.render_filter_menu(false, cx)).with_priority(1))
            })
            .when(self.sidebar.switch_hovered, |sidebar| {
                sidebar.child(filter::switch_tip("Show threads"))
            })
            .test_support()
    }

    fn render_sidebar_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.sidebar.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        let el = match row {
            Row::Heading(heading) => render_heading(heading).into_any_element(),
            Row::Project(look) => self.render_project(look, cx),
            Row::Channel(channel) => self.render_channel(channel, cx),
            Row::TerminalGroup(project) => self.render_terminal_group(project, cx),
            Row::TerminalSession(id) => self.render_terminal_session(&id, cx),
            Row::Other { count, open } => render_other(count, open, cx).into_any_element(),
            Row::ChannelHit { channel, hit } => self.render_channel_hit(channel, hit, cx),
            Row::NoMatches => div()
                .id("sidebar-no-matches")
                .px(ui_px(8.))
                .py(scale::row(6.))
                .text_size(scale::text(12.))
                .text_color(color("text-tertiary"))
                .aria_label("No projects or channels match.")
                .child("No projects or channels match.")
                .test_support()
                .into_any_element(),
            Row::Divider => div()
                .my(scale::row(10.))
                .mx(ui_px(8.))
                .h(ui_px(1.))
                .bg(color("border-100"))
                .into_any_element(),
            Row::AgentsHeading => self.render_agents_heading(cx).into_any_element(),
            Row::Agent(agent) => match self.state().agent(agent) {
                Some(agent) => self.render_agent(agent).into_any_element(),
                None => div().into_any_element(),
            },
        };
        div()
            .px(ui_px(8.))
            .pb(ui_px(1.))
            .child(el)
            .into_any_element()
    }

    fn render_channel(&self, channel: ChannelId, cx: &mut Context<Self>) -> AnyElement {
        let Some(name) = self.state().channel(channel).map(|c| c.name.clone()) else {
            return div().into_any_element();
        };
        #[cfg(test)]
        row_probe::own(channel);
        let selected = self.nav.channel == Some(channel);
        let badge = self.channel_badge(channel);
        channel_row(
            format!("channel-{channel}"),
            name,
            selected || matches!(badge, ChannelBadge::Unread(_)),
            selected,
            format!("channel-label-{channel}"),
        )
        .child(render_badge(format!("channel-status-{channel}"), &badge))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.select_channel(channel, window, cx);
        }))
        .test_support()
        .into_any_element()
    }

    /// A channel the filter found: `#name`, "in {project}" and its
    /// status.
    fn render_channel_hit(
        &self,
        channel: ChannelId,
        hit: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.state();
        let Some(stored) = state.channel(channel) else {
            return div().into_any_element();
        };
        let project = state
            .project(stored.project)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let name = stored.name.clone();
        let range = match_range(&name, self.sidebar.query.trim());
        let selected = hit == self.sidebar.selected;
        let badge = self.channel_badge(channel);
        sidebar_row(format!("channel-hit-{channel}"))
            .gap(ui_px(8.))
            .when(selected, |row| row.bg(color("surface-300")))
            .text_color(color("text-secondary"))
            .aria_label(format!("#{name} in {project}"))
            .aria_selected(selected)
            .child(
                div()
                    .flex_none()
                    .font_family(code_family())
                    .text_size(scale::mono(13.))
                    .text_color(color("text-gutter"))
                    .child("#"),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(color("text-primary"))
                    .child(highlighted(&name, range)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .child(format!("in {project}")),
            )
            .child(render_badge(
                format!("channel-hit-status-{channel}"),
                &badge,
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_channel(channel, window, cx);
            }))
            .test_support()
            .into_any_element()
    }

    /// "In terminal · N" under a project with terminal sessions.
    fn render_terminal_group(&self, project: ProjectId, cx: &mut Context<Self>) -> AnyElement {
        let sessions = self.state().terminal_sessions(project).len();
        let open = self.nav.terminal_groups.contains(&project);
        let label = format!("In terminal · {sessions}");
        sidebar_row(format!("terminal-group-{project}"))
            .pl(ui_px(28.))
            .gap(ui_px(4.))
            .text_color(color("text-secondary"))
            .aria_label(label.clone())
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(ui_px(12.))
                .text_color(color("text-tertiary")),
            )
            .child(div().flex_1().min_w_0().truncate().child(label))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_terminal_group(project, cx);
            }))
            .test_support()
            .into_any_element()
    }

    fn render_terminal_session(&self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let Some(session) = self.state().terminal_session(id).cloned() else {
            return div().into_any_element();
        };
        render_session(
            &session,
            self.terminal.as_ref().map(|t| t.session.as_str()),
            cx,
        )
    }

    fn toggle_terminal_group(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if !self.nav.terminal_groups.remove(&project) {
            self.nav.terminal_groups.insert(project);
        }
        self.sync_terminal_ticker(cx);
        cx.notify();
    }

    /// The badge of `channel`'s row: urgent states first, then unread
    /// messages. The open channel has none unread.
    pub(super) fn channel_badge(&self, channel: ChannelId) -> ChannelBadge {
        let state = self.state();
        match state.channel_status(channel) {
            ThreadStatus::Idle | ThreadStatus::Queued => {}
            status => return ChannelBadge::Status(status),
        }
        if self.nav.channel == Some(channel) {
            return ChannelBadge::None;
        }
        let since = self.nav.viewed.get(&channel).copied().unwrap_or(0);
        match state.unread(channel, since) {
            0 => ChannelBadge::None,
            n => ChannelBadge::Unread(n),
        }
    }

    /// "Agents", which opens and closes the list.
    fn render_agents_heading(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.sidebar.agents_open;
        sidebar_row("sidebar-agents".into())
            .gap(ui_px(8.))
            .pr(ui_px(4.))
            .aria_label("Agents")
            .aria_expanded(open)
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(ui_px(12.))
                .text_color(color("text-secondary")),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(scale::text(12.))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color("text-secondary"))
                    .child("Agents"),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.sidebar.agents_open = !this.sidebar.agents_open;
                cx.notify();
            }))
            .test_support()
    }

    fn render_agent(&self, agent: &StoredAgent) -> impl IntoElement {
        let id = agent.id;
        let status = self.state().agent_status(id);
        let (word, token) = status_word(status);
        sidebar_row(format!("agent-{id}"))
            .gap(ui_px(8.))
            .aria_label(agent.spec.name.clone())
            .child(monogram(&agent.spec.name, 20.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(color(if status == ThreadStatus::Idle {
                        "text-secondary"
                    } else {
                        "text-primary"
                    }))
                    .child(agent.spec.name.clone()),
            )
            .child(
                div()
                    .id(format!("agent-status-{id}"))
                    .flex_none()
                    .text_size(scale::text(11.))
                    .text_color(color(token))
                    .aria_label(word)
                    .child(word)
                    .test_support(),
            )
            .test_support()
    }

    pub(super) fn toggle_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if !self.nav.collapsed.remove(&project) {
            self.nav.collapsed.insert(project);
        }
        self.sync_terminal_ticker(cx);
        cx.notify();
    }

    /// Opens or closes Other projects.
    fn toggle_other(&mut self, cx: &mut Context<Self>) {
        self.sidebar.other_open = !self.sidebar.other_open;
        cx.notify();
    }

    /// Saves the filter menu's choices, off the UI thread, after any
    /// write still pending.
    pub(super) fn save_sidebar_prefs(&mut self, cx: &mut Context<Self>) {
        let prefs = self.sidebar.prefs;
        let home = self.conn.crew_home.clone();
        let pending = self.sidebar.prefs_task.take();
        self.sidebar.prefs_task = Some(cx.background_executor().spawn(async move {
            if let Some(pending) = pending {
                pending.await;
            }
            if let Err(e) = prefs.save(&home) {
                eprintln!("crew: couldn't save the sidebar choices: {e}");
            }
        }));
    }
}

/// A terminal session under its project.
fn render_session(
    session: &TerminalSession,
    open: Option<&str>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let id = session.id.clone();
    let selected = open == Some(id.as_str());
    let title = if session.title.is_empty() {
        "Untitled session".to_string()
    } else {
        session.title.clone()
    };
    let target = id.clone();
    sidebar_row(format!("terminal-session-{id}"))
        .pl(ui_px(44.))
        .gap(ui_px(6.))
        .when(selected, |row| row.bg(color("surface-300")))
        .text_color(color(if selected {
            "text-primary"
        } else {
            "text-secondary"
        }))
        .aria_label(title.clone())
        .child(div().flex_1().min_w_0().truncate().child(title))
        .child(
            div()
                .flex_none()
                .text_size(scale::text(11.))
                .text_color(color("text-tertiary"))
                .child(ago(session.last_active_at, now_ms())),
        )
        .when(session.live, |row| {
            row.child(
                div()
                    .id(SharedString::from(format!("terminal-live-{id}")))
                    .flex_none()
                    .p(ui_px(2.))
                    .aria_label("live")
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("live").build(window, cx)
                    })
                    .child(dot("accent-blue"))
                    .test_support(),
            )
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_terminal(target.clone(), window, cx);
        }))
        .test_support()
        .into_any_element()
}

/// "Pinned", "Active", "Projects · 3" or "Channels · 1".
fn render_heading(heading: Heading) -> impl IntoElement {
    let text = heading.text();
    div()
        .id(SharedString::from(format!(
            "sidebar-group-{}",
            heading.key()
        )))
        .flex()
        .items_center()
        .gap(ui_px(6.))
        .h(scale::row(24.))
        .mt(scale::row(6.))
        .px(ui_px(8.))
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-tertiary"))
        .aria_label(text.clone())
        .when(heading == Heading::Pinned, |h| {
            h.child(
                Icon::default()
                    .data(PIN_SVG)
                    .size(ui_px(11.))
                    .text_color(color("text-tertiary")),
            )
        })
        .child(text)
        .test_support()
}

/// "Other projects · {n}", which opens and closes the rest.
fn render_other(count: usize, open: bool, cx: &mut Context<Workspace>) -> impl IntoElement {
    let text = format!("Other projects · {count}");
    sidebar_row("sidebar-other".into())
        .mt(scale::row(6.))
        .gap(ui_px(8.))
        .aria_label(text.clone())
        .aria_expanded(open)
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(ui_px(12.))
            .text_color(color("text-tertiary")),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(scale::text(12.))
                .font_weight(weight(FontWeight::MEDIUM))
                .text_color(color("text-tertiary"))
                .child(text),
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_other(cx)))
        .test_support()
}

/// A project's row as the appearance preview draws it: a chevron, open
/// or closed, and its name.
pub(super) fn project_row(id: String, name: String, open: bool) -> Stateful<Div> {
    sidebar_row(id)
        .gap(ui_px(8.))
        .text_color(color("text-primary"))
        .aria_label(name.clone())
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(ui_px(12.))
            .text_color(color("text-secondary")),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(weight(FontWeight::MEDIUM))
                .child(name),
        )
}

/// A channel's row under its project: `#` and its name, brighter when
/// `strong` (open, or with unread messages). Tests read the name's text
/// size under `probe`.
pub(super) fn channel_row(
    id: String,
    name: String,
    strong: bool,
    selected: bool,
    probe: String,
) -> Stateful<Div> {
    sidebar_row(id)
        .pl(ui_px(28.))
        .gap(ui_px(8.))
        .when(selected, |row| row.bg(color("surface-300")))
        .text_color(color(if strong {
            "text-primary"
        } else {
            "text-secondary"
        }))
        .aria_label(format!("#{name}"))
        .aria_selected(selected)
        .child(
            div()
                .flex_none()
                .font_family(code_family())
                .text_size(scale::mono(13.))
                .text_color(color(if selected {
                    "text-secondary"
                } else {
                    "text-gutter"
                }))
                .child("#"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(strong, |name| name.font_weight(weight(FontWeight::MEDIUM)))
                .child(name)
                .map(|label| with_font_probe(label, || probe)),
        )
}

/// A 28 px sidebar row.
fn sidebar_row(id: String) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .flex()
        .items_center()
        .h(scale::row(28.))
        .px(ui_px(8.))
        .rounded(ui_px(6.))
        .cursor_pointer()
        .whitespace_nowrap()
        .hover(|row| row.bg(color("surface-200")))
}

/// A dot for urgent states only: queued runs get none.
fn status_dot(status: ThreadStatus) -> Option<Div> {
    match status {
        ThreadStatus::Idle | ThreadStatus::Queued => None,
        status => Some(dot(status_word(status).1)),
    }
}

/// A status dot or an unread count, labelled for tests under `id`.
fn render_badge(id: String, badge: &ChannelBadge) -> impl IntoElement {
    let el = div()
        .id(SharedString::from(id))
        .flex_none()
        .aria_label(badge.label());
    match badge {
        ChannelBadge::Status(status) => el.when_some(status_dot(*status), |el, dot| el.child(dot)),
        ChannelBadge::Unread(n) => el
            .px(ui_px(6.))
            .rounded_full()
            .bg(color("surface-300"))
            .text_size(scale::text(11.))
            .font_weight(weight(FontWeight::MEDIUM))
            .text_color(color("text-primary"))
            .child(n.to_string()),
        ChannelBadge::None => el,
    }
    .test_support()
}

/// "N agents running" and Stop all.
fn render_footer(running: usize, stopping: bool, cx: &mut Context<Workspace>) -> impl IntoElement {
    let agents = if running == 1 { "agent" } else { "agents" };
    div()
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(44.))
        .pl(ui_px(16.))
        .pr(ui_px(12.))
        .border_t_1()
        .border_color(color("border-100"))
        .text_size(scale::text(12.))
        .text_color(color("text-secondary"))
        .child(dot("accent-blue"))
        .child({
            let text = format!("{running} {agents} running");
            div()
                .id("sidebar-running")
                .flex_1()
                .aria_label(text.clone())
                .child(text)
                .test_support()
        })
        .child(
            Button::new("sidebar-stop-all")
                .outline()
                .xsmall()
                .text_color(color(super::running_view::stop_all_look(true, stopping).1))
                .icon(IconName::Square)
                .label("Stop all")
                .disabled(stopping)
                .on_click(cx.listener(|this, _, window, cx| this.stop_all(window, cx))),
        )
}

/// Which channels a project's single row stands for, as last drawn, so
/// tests find the row that opens a channel.
#[cfg(test)]
pub(crate) mod row_probe {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use crew_core::ids::{ChannelId, ProjectId};

    thread_local! {
        static SINGLE: RefCell<HashMap<ChannelId, ProjectId>> = RefCell::new(HashMap::new());
    }

    pub(crate) fn single(channel: ChannelId, project: ProjectId) {
        SINGLE.with(|s| s.borrow_mut().insert(channel, project));
    }

    pub(crate) fn own(channel: ChannelId) {
        SINGLE.with(|s| s.borrow_mut().remove(&channel));
    }

    /// The id of the row that opens `channel`: its own, or its project's
    /// when that row stands for it.
    pub(crate) fn row_of(channel: ChannelId) -> String {
        match SINGLE.with(|s| s.borrow().get(&channel).copied()) {
            Some(project) => format!("project-{project}"),
            None => format!("channel-{channel}"),
        }
    }

    /// The id of the status that row shows.
    pub(crate) fn status_of(channel: ChannelId) -> String {
        match SINGLE.with(|s| s.borrow().get(&channel).copied()) {
            Some(project) => format!("project-status-{project}"),
            None => format!("channel-status-{channel}"),
        }
    }
}
