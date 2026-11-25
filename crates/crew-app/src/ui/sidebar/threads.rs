//! The sidebar's Threads form: every project's threads in one list,
//! grouped Needs you, Working and Recent, with the quiet ones under
//! Settled. Rows sit in a virtual list, so many threads stay smooth.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crew_core::ids::{ChannelId, ProjectId, ThreadId};
use crew_core::store::ApprovalKind;
use crew_proto::{Request, SearchResults};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::channel_view::{CardStatus, card_status};
use super::super::common::{elapsed, now_ms, provider_icon};
use super::super::palette::{highlighted, match_range};
use super::super::workspace::{Place, Workspace};
use super::filter::{COMPOSE_SVG, FOLDER_PLUS_SVG, FUNNEL_SVG, icon_button, switch_button};
use super::projects::{ACTIVE_WITHIN_MS, group_projects};
use crate::model::{ThreadFacts, ThreadStatus};
use crate::prefs::SidebarMode;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

actions!(
    threads_sidebar,
    [
        /// The next thread in the list.
        ThreadsDown,
        /// The thread above.
        ThreadsUp,
        /// Opens the highlighted thread.
        ThreadsOpen,
        /// Empties the search.
        ThreadsClear,
        /// Leaves the list for the search.
        ThreadsBack,
    ]
);

/// How long typing must pause before crewd searches messages.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
/// Most threads a message search brings.
const SEARCH_LIMIT: u32 = 200;

/// Needs you: a circle with a bang.
const ALERT_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="8" cy="8" r="6"/><path d="M8 5v3.5M8 11v.5"/></svg>"#;
/// Working: an open ring.
const ARC_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M8 2a6 6 0 1 1-6 6"/></svg>"#;
/// Done: a check.
const DONE_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 8.5l3 3 7-7"/></svg>"#;
/// Stopped: a square.
const STOP_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="currentColor"><rect x="3.5" y="3.5" width="9" height="9" rx="1.5"/></svg>"#;
/// Failed: a cross.
const FAILED_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/></svg>"#;
/// Show projects: a tree.
const TREE_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M2.5 2.5h5M4.5 2.5v11M4.5 8h9M4.5 13.5h9"/></svg>"#;

/// What a thread is grouped by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThreadInfo {
    pub(crate) id: ThreadId,
    pub(crate) status: ThreadStatus,
    /// When something last happened in it, in Unix ms.
    pub(crate) activity: i64,
}

/// The list's groups, newest activity first in each.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ThreadGroups {
    pub(crate) needs_you: Vec<ThreadId>,
    /// Working, or waiting for a slot.
    pub(crate) working: Vec<ThreadId>,
    /// Neither, with activity in the last 7 days.
    pub(crate) recent: Vec<ThreadId>,
    /// Neither, and quiet for longer.
    pub(crate) settled: Vec<ThreadId>,
}

/// Puts each thread in Needs you, Working, Recent or Settled.
pub(crate) fn group_threads(mut threads: Vec<ThreadInfo>, now: i64) -> ThreadGroups {
    threads.sort_by(|a, b| b.activity.cmp(&a.activity).then(a.id.0.cmp(&b.id.0)));
    let mut groups = ThreadGroups::default();
    for t in threads {
        match t.status {
            ThreadStatus::NeedsYou => groups.needs_you.push(t.id),
            ThreadStatus::Working | ThreadStatus::Queued => groups.working.push(t.id),
            ThreadStatus::Idle if now - t.activity <= ACTIVE_WITHIN_MS => {
                groups.recent.push(t.id);
            }
            ThreadStatus::Idle => groups.settled.push(t.id),
        }
    }
    groups
}

/// A project's badge: its letters and its tint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Badge {
    pub(crate) letters: String,
    pub(crate) tint: usize,
}

/// Each project's two-letter badge: the first letters of its first two
/// words, else its first two letters, else its first and last, else its
/// first and another, so no two badges are the same while that can be.
/// Names of more than one word go first. `projects` come in the order
/// they were added, which hands out the tints in turn.
pub(crate) fn badges(projects: &[(ProjectId, &str)]) -> HashMap<ProjectId, Badge> {
    let tints: HashMap<ProjectId, usize> = projects
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (*id, i % TINTS))
        .collect();
    let mut order: Vec<&(ProjectId, &str)> = projects.iter().collect();
    order.sort_by_key(|(_, name)| usize::from(words(name).len() < 2));
    let mut taken: HashSet<String> = HashSet::new();
    let mut out = HashMap::new();
    for (id, name) in order {
        let candidates = badge_candidates(name);
        let pick = candidates
            .iter()
            .find(|c| !taken.contains(*c))
            .or(candidates.first())
            .cloned()
            .unwrap_or_default();
        taken.insert(pick.clone());
        let tint = tints[id];
        out.insert(
            *id,
            Badge {
                letters: pick,
                tint,
            },
        );
    }
    out
}

fn words(name: &str) -> Vec<Vec<char>> {
    name.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.chars().flat_map(char::to_uppercase).collect())
        .collect()
}

fn badge_candidates(name: &str) -> Vec<String> {
    let words = words(name);
    let mut out: Vec<String> = Vec::new();
    if let [a, b, ..] = words.as_slice() {
        out.push([a[0], b[0]].iter().collect());
    }
    let letters: Vec<char> = words.concat();
    match letters.as_slice() {
        [] => {}
        [only] => out.push(only.to_string()),
        [first, rest @ ..] => {
            out.push([*first, rest[0]].iter().collect());
            if let Some(last) = rest.last() {
                out.push([*first, *last].iter().collect());
            }
            out.extend(rest.iter().map(|c| [*first, *c].iter().collect::<String>()));
        }
    }
    out
}

/// The tints badges take in turn, in the order projects were added:
/// blue, green, purple, cyan, then neutral.
const TINTS: usize = 5;

/// The colours of badge tint `tint`: a 15% wash and the text on it.
pub(crate) fn badge_tint(tint: usize) -> (Hsla, Hsla) {
    let (wash, text) = match tint % TINTS {
        0 => (color("info-subtle"), color("accent-blue")),
        1 => (color("success-subtle"), color("success")),
        2 => (color("accent-purple").opacity(0.15), color("accent-purple")),
        3 => (color("brand-cyan").opacity(0.15), color("brand-cyan")),
        _ => (color("surface-300"), color("text-primary")),
    };
    (wash, text)
}

/// `45s`, `22m` or `1h 3m`: how long agents worked, roughly.
pub(crate) fn span(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h {}m", s / 3600, s / 60 % 60),
    }
}

/// What an ask wants, after the agent's name: "wants to run a command".
pub(crate) fn wants(
    kind: &ApprovalKind,
    name_of: impl Fn(crew_core::ids::AgentId) -> String,
) -> String {
    match kind {
        ApprovalKind::Handoff { to, .. } => format!("wants to bring in {}", name_of(*to)),
        ApprovalKind::Permission { tool, .. } | ApprovalKind::Blocked { tool, .. } => {
            if tool == "Bash" {
                "wants to run a command".into()
            } else {
                format!("wants to run {}", crew_core::event::tool_label(tool))
            }
        }
    }
}

pub(super) fn plural(n: impl Into<u64>, one: &str, many: &str) -> String {
    let n = n.into();
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A group heading of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Group {
    NeedsYou,
    Working,
    Recent,
}

impl Group {
    fn key(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs",
            Self::Working => "working",
            Self::Recent => "recent",
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Working => "Working",
            Self::Recent => "Recent",
        }
    }
}

/// One row of the threads list.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TRow {
    Heading(Group, usize),
    Thread(ThreadId),
    /// "Settled · {n}", open or closed.
    Settled {
        count: usize,
        open: bool,
    },
    Empty,
}

/// What the rows depend on: they are built again only when it changes.
#[derive(Debug, Clone, PartialEq)]
struct ThreadsKey {
    revision: u64,
    summaries: u64,
    query: String,
    remote: u64,
    project: Option<ProjectId>,
    channel: Option<ChannelId>,
    show: crate::prefs::ShowProjects,
    settled_open: bool,
    settled_found_open: bool,
    open: Option<ThreadId>,
    minute: i64,
}

/// The threads sidebar's own state, kept in memory.
pub(crate) struct ThreadsSidebar {
    pub(crate) search: Entity<InputState>,
    pub(super) query: String,
    /// The highlighted thread, an index into `order`.
    pub(crate) cursor: Option<usize>,
    pub(super) list: ListState,
    pub(super) rows: Vec<TRow>,
    /// The thread rows, in list order.
    pub(crate) order: Vec<ThreadId>,
    built: Option<ThreadsKey>,
    pub(super) facts: HashMap<ThreadId, ThreadFacts>,
    pub(super) badges: HashMap<ProjectId, Badge>,
    pub(crate) settled_open: bool,
    /// Settled is open among search results: it starts open for each
    /// new search, and can be closed.
    settled_found_open: bool,
    /// The project the switcher picked, if one.
    pub(crate) project: Option<ProjectId>,
    /// The channel chip picked, if one.
    pub(crate) channel: Option<ChannelId>,
    pub(super) switcher: super::switcher::Switcher,
    /// What crewd last said about each thread.
    pub(super) summaries: super::summaries::Summaries,
    /// Threads whose messages hold the query, as crewd last said.
    remote: Option<(String, HashSet<ThreadId>)>,
    remote_gen: u64,
    search_task: Option<Task<()>>,
    /// The thread whose popover shows, and its row.
    pub(crate) hovered: Option<(ThreadId, usize)>,
    pub(super) popover_hovered: bool,
    /// The list's own focus, for its keys.
    pub(crate) list_focus: FocusHandle,
    pub(super) linger: Option<Task<()>>,
    /// The open thread the list last made visible.
    revealed: Option<ThreadId>,
    /// The time grouping uses in place of now, as tests set it.
    #[cfg(test)]
    pub(crate) now: Option<i64>,
    _search_events: Subscription,
}

impl ThreadsSidebar {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search threads"));
        let events = cx.subscribe_in(&search, window, Workspace::thread_search_changed);
        Self {
            search,
            query: String::new(),
            cursor: None,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            rows: Vec::new(),
            order: Vec::new(),
            built: None,
            facts: HashMap::new(),
            badges: HashMap::new(),
            settled_open: false,
            settled_found_open: true,
            project: None,
            channel: None,
            switcher: super::switcher::Switcher::new(window, cx),
            summaries: Default::default(),
            remote: None,
            remote_gen: 0,
            search_task: None,
            hovered: None,
            popover_hovered: false,
            list_focus: cx.focus_handle(),
            linger: None,
            revealed: None,
            #[cfg(test)]
            now: None,
            _search_events: events,
        }
    }

    /// The list shows again after a switch: it shows the open thread,
    /// and summaries that changed meanwhile are asked for at once.
    pub(super) fn shown_again(&mut self) {
        self.revealed = None;
        self.built = None;
        self.summaries.shown_again();
    }
}

/// A sidebar mode shown for now without saving it, until the saved mode
/// changes or you switch.
struct ShownMode {
    mode: SidebarMode,
    /// The saved mode when it was shown.
    over: SidebarMode,
}

impl Global for ShownMode {}

/// The mode the sidebar is in.
pub(crate) fn sidebar_mode(cx: &App) -> SidebarMode {
    let saved = scale::appearance(cx).sidebar_mode;
    match cx.try_global::<ShownMode>() {
        Some(shown) if shown.over == saved => shown.mode,
        _ => saved,
    }
}

impl Workspace {
    /// Switches the sidebar to the other form, and saves that.
    pub(super) fn switch_sidebar(&mut self, cx: &mut Context<Self>) {
        let want = match sidebar_mode(cx) {
            SidebarMode::Projects => SidebarMode::Threads,
            SidebarMode::Threads => SidebarMode::Projects,
        };
        if cx.has_global::<ShownMode>() {
            cx.remove_global::<ShownMode>();
        }
        let mut a = scale::appearance(cx);
        if a.sidebar_mode != want {
            a.sidebar_mode = want;
            scale::set_appearance(a, cx);
        }
        cx.notify();
    }

    /// The threads sidebar, narrowed to `project`'s threads in all its
    /// channels. The mode isn't saved: it is a look, not a choice.
    pub(crate) fn show_project_threads(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        let t = &mut self.sidebar.threads;
        t.project = Some(project);
        t.channel = None;
        t.cursor = None;
        t.list.scroll_to(ListOffset::default());
        let saved = scale::appearance(cx).sidebar_mode;
        if saved == SidebarMode::Threads {
            if cx.has_global::<ShownMode>() {
                cx.remove_global::<ShownMode>();
            }
        } else {
            cx.set_global(ShownMode {
                mode: SidebarMode::Threads,
                over: saved,
            });
        }
        cx.notify();
    }

    pub(super) fn threads_now(&self) -> i64 {
        #[cfg(test)]
        if let Some(now) = self.sidebar.threads.now {
            return now;
        }
        now_ms()
    }

    /// The search field changed: the list shows what it finds, and crewd
    /// looks in the threads' messages once typing pauses.
    pub(super) fn thread_search_changed(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let query = input.read(cx).value().to_string();
        if query == self.sidebar.threads.query {
            return;
        }
        let t = &mut self.sidebar.threads;
        t.query = query.clone();
        t.settled_found_open = true;
        t.cursor = None;
        t.list.scroll_to(ListOffset::default());
        let query = query.trim().to_string();
        if query.is_empty() {
            t.search_task = None;
            t.remote = None;
            cx.notify();
            return;
        }
        let project = t.project;
        let client = self.client.clone();
        self.sidebar.threads.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let answer = client
                .request(Request::Search {
                    query: query.clone(),
                    project,
                    limit: SEARCH_LIMIT,
                })
                .await
                .and_then(|v| {
                    serde_json::from_value::<SearchResults>(v).map_err(|e| e.to_string())
                });
            this.update(cx, |this, cx| {
                let t = &mut this.sidebar.threads;
                t.search_task = None;
                match answer {
                    Ok(found) if t.query.trim() == query => {
                        let ids = found.threads.into_iter().map(|h| h.thread).collect();
                        t.remote = Some((query, ids));
                        t.remote_gen += 1;
                        cx.notify();
                    }
                    Ok(_) => {}
                    // Names still match; messages don't for now.
                    Err(e) => eprintln!("crew: thread search failed: {e}"),
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Builds the threads list's rows from what is known, keeping the
    /// scroll position when they change.
    pub(super) fn refresh_threads_sidebar(&mut self) {
        let now = self.threads_now();
        let open = self.nav.thread;
        let key = ThreadsKey {
            revision: self.state().revision(),
            summaries: self.sidebar.threads.summaries.generation,
            query: self.sidebar.threads.query.trim().to_lowercase(),
            remote: self.sidebar.threads.remote_gen,
            project: self.sidebar.threads.project,
            channel: self.sidebar.threads.channel,
            show: self.sidebar.prefs.show,
            settled_open: self.sidebar.threads.settled_open,
            settled_found_open: self.sidebar.threads.settled_found_open,
            open,
            minute: now / 60_000,
        };
        if self.sidebar.threads.built.as_ref() == Some(&key) {
            return;
        }
        let facts = self.state().thread_facts();
        let names: Vec<(ProjectId, &str)> = self
            .state()
            .projects()
            .into_iter()
            .map(|p| (p.id, p.name.as_str()))
            .collect();
        let badges = badges(&names);
        let groups = self.thread_groups(&facts, &key.query, now);
        // The open thread shows: Settled opens if it is there.
        let reveal = open.filter(|o| self.sidebar.threads.revealed != Some(*o));
        if let Some(o) = reveal {
            self.sidebar.threads.revealed = Some(o);
            if groups.settled.contains(&o) {
                self.sidebar.threads.settled_open = true;
            }
        }
        let searching = !key.query.is_empty();
        let settled_open = if searching {
            self.sidebar.threads.settled_found_open
        } else {
            self.sidebar.threads.settled_open
        };
        let mut rows = Vec::new();
        let mut order = Vec::new();
        for (group, ids) in [
            (Group::NeedsYou, &groups.needs_you),
            (Group::Working, &groups.working),
            (Group::Recent, &groups.recent),
        ] {
            if ids.is_empty() {
                continue;
            }
            rows.push(TRow::Heading(group, ids.len()));
            rows.extend(ids.iter().map(|t| TRow::Thread(*t)));
            order.extend(ids.iter().copied());
        }
        if !groups.settled.is_empty() {
            rows.push(TRow::Settled {
                count: groups.settled.len(),
                open: settled_open,
            });
            if settled_open {
                rows.extend(groups.settled.iter().map(|t| TRow::Thread(*t)));
                order.extend(groups.settled.iter().copied());
            }
        }
        if rows.is_empty() {
            rows.push(TRow::Empty);
        }
        let t = &mut self.sidebar.threads;
        t.built = Some(ThreadsKey {
            settled_open: t.settled_open,
            ..key
        });
        t.facts = facts;
        t.badges = badges;
        // Typing highlights the first thread found.
        t.cursor = match t.cursor {
            Some(c) if c < order.len() => Some(c),
            _ if searching && !order.is_empty() => Some(0),
            _ => None,
        };
        let open_row = reveal.and_then(|o| rows.iter().position(|r| *r == TRow::Thread(o)));
        t.order = order;
        if rows != t.rows {
            // The popover follows its thread's row, or goes with it.
            t.hovered = t.hovered.and_then(|(h, _)| {
                rows.iter()
                    .position(|r| *r == TRow::Thread(h))
                    .map(|ix| (h, ix))
            });
            let top = t.list.logical_scroll_top();
            t.list.reset(rows.len());
            t.list.scroll_to(ListOffset {
                item_ix: top.item_ix.min(rows.len().saturating_sub(1)),
                offset_in_item: top.offset_in_item,
            });
            t.rows = rows;
        }
        if let Some(row) = open_row {
            t.list.scroll_to_reveal_item(row);
        }
    }

    /// The projects whose threads list, as Show has it; the open
    /// thread's project always does.
    pub(super) fn shown_projects(&self, now: i64) -> HashSet<ProjectId> {
        let state = self.state();
        let open_project = self.nav.thread.and_then(|o| state.thread_project(o));
        let project_facts = state.project_facts();
        let infos = state
            .projects()
            .into_iter()
            .map(|p| super::project_info(p, project_facts.get(&p.id)))
            .collect();
        let shown = group_projects(
            infos,
            self.sidebar.prefs.show,
            self.sidebar.prefs.sort,
            now,
            open_project,
        );
        shown
            .pinned
            .into_iter()
            .chain(shown.active)
            .chain(shown.other)
            .collect()
    }

    /// The threads to list, grouped: not archived, in the projects shown
    /// and the picked project and channel, matching the search.
    fn thread_groups(
        &self,
        facts: &HashMap<ThreadId, ThreadFacts>,
        query: &str,
        now: i64,
    ) -> ThreadGroups {
        let state = self.state();
        let t = &self.sidebar.threads;
        let shown = self.shown_projects(now);
        let remote = t
            .remote
            .as_ref()
            .filter(|(q, _)| q.to_lowercase() == query)
            .map(|(_, ids)| ids);
        let mut items = Vec::new();
        for project in state.projects() {
            if !shown.contains(&project.id) || t.project.is_some_and(|p| p != project.id) {
                continue;
            }
            for channel in state.channels(project.id) {
                if t.channel.is_some_and(|c| c != channel.id) {
                    continue;
                }
                for thread in state.threads(channel.id) {
                    if !query.is_empty() {
                        let branch = thread.worktree.as_ref().map(|w| w.branch.as_str());
                        let local = [
                            Some(thread.title.as_str()),
                            Some(project.name.as_str()),
                            Some(channel.name.as_str()),
                            branch,
                        ]
                        .into_iter()
                        .flatten()
                        .any(|text| text.to_lowercase().contains(query));
                        if !local && !remote.is_some_and(|r| r.contains(&thread.id)) {
                            continue;
                        }
                    }
                    let f = facts.get(&thread.id);
                    let summary = t
                        .summaries
                        .by_thread
                        .get(&thread.id)
                        .map_or(0, |s| s.last_activity_at);
                    items.push(ThreadInfo {
                        id: thread.id,
                        status: f.map_or(ThreadStatus::Idle, |f| f.status),
                        activity: f.map_or(thread.created_at, |f| f.activity).max(summary),
                    });
                }
            }
        }
        group_threads(items, now)
    }

    /// The thread's card status: its ask, its runs, else how its latest
    /// run ended, from what is known here or crewd's summary.
    pub(super) fn thread_card_status(&self, thread: ThreadId) -> CardStatus {
        let t = &self.sidebar.threads;
        let facts = t.facts.get(&thread);
        let status = facts.map_or(ThreadStatus::Idle, |f| f.status);
        let summary_last = t
            .summaries
            .by_thread
            .get(&thread)
            .and_then(|s| s.runs.last())
            .map(|r| r.status.clone());
        let last = facts.and_then(|f| f.last_run.clone()).or(summary_last);
        card_status(
            status == ThreadStatus::NeedsYou,
            status == ThreadStatus::Working,
            status == ThreadStatus::Queued,
            last.as_ref(),
        )
    }

    /// ↓ or ↑ in the search: the next or previous thread.
    fn move_thread_cursor(&mut self, by: isize, cx: &mut Context<Self>) {
        let t = &mut self.sidebar.threads;
        let count = t.order.len();
        if count == 0 {
            return;
        }
        let start = t.cursor.or_else(|| {
            self.nav
                .thread
                .and_then(|o| t.order.iter().position(|x| *x == o))
        });
        let next = match start {
            None if by > 0 => 0,
            None => count - 1,
            Some(c) => c.saturating_add_signed(by).min(count - 1),
        };
        t.cursor = Some(next);
        let thread = t.order[next];
        if let Some(row) = t.rows.iter().position(|r| *r == TRow::Thread(thread)) {
            t.list.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    /// ↓ or ↑ in the search: the list takes the keys from there.
    fn move_into_list(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.move_thread_cursor(by, cx);
        if self.sidebar.threads.cursor.is_some() {
            self.sidebar.threads.list_focus.focus(window, cx);
        }
    }

    /// ↵ in the search or the list: opens the highlighted thread. The
    /// keys stay where they were, so you can go on.
    fn open_thread_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t = &self.sidebar.threads;
        let Some(thread) = t.cursor.and_then(|c| t.order.get(c)).copied() else {
            return;
        };
        let in_list = t.list_focus.is_focused(window);
        self.open_place(Place::Thread(thread), window, cx);
        if in_list {
            self.sidebar.threads.list_focus.focus(window, cx);
        } else {
            self.sidebar
                .threads
                .search
                .update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Esc in the list: back to the search.
    fn back_to_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.threads.cursor = None;
        self.sidebar
            .threads
            .search
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Esc in the search: empties it, or leaves it when it is empty.
    fn clear_thread_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar.threads.query.is_empty() {
            self.sidebar.threads.cursor = None;
            self.focus_workspace(window, cx);
            cx.notify();
            return;
        }
        self.sidebar
            .threads
            .search
            .update(cx, |input, cx| input.set_value("", window, cx));
        let t = &mut self.sidebar.threads;
        t.query.clear();
        t.remote = None;
        t.search_task = None;
        t.cursor = None;
        cx.notify();
    }

    /// The threads sidebar: its bar, the project switcher and chips, the
    /// list and the footer.
    pub(super) fn render_threads_sidebar(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let t = &self.sidebar.threads;
        let switcher_open = t.switcher.open;
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
            .aria_label("Threads")
            .child(self.render_threads_bar(cx))
            .child(self.render_switcher_button(cx))
            .children(self.render_channel_chips(cx))
            .child(
                div()
                    .id("threads-list")
                    .key_context("ThreadsList")
                    .track_focus(&t.list_focus)
                    .flex_1()
                    .min_h_0()
                    .on_action(
                        cx.listener(|this, _: &ThreadsDown, _, cx| this.move_thread_cursor(1, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &ThreadsUp, _, cx| this.move_thread_cursor(-1, cx)),
                    )
                    .on_action(cx.listener(|this, _: &ThreadsOpen, window, cx| {
                        this.open_thread_cursor(window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &ThreadsBack, window, cx| {
                        this.back_to_search(window, cx)
                    }))
                    // A press in the list gives it the keys; a row's click
                    // may then take them for what it opens.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.sidebar.threads.list_focus.focus(window, cx);
                        }),
                    )
                    .when(self.sidebar.menu_open, |list| list.opacity(0.4))
                    .child(
                        list(
                            t.list.clone(),
                            cx.processor(|this, ix, _, cx| this.render_threads_row(ix, cx)),
                        )
                        .size_full(),
                    )
                    .test_support(),
            )
            .child(self.render_threads_footer(cx))
            .when(switcher_open, |sidebar| {
                sidebar.child(deferred(self.render_switcher(cx)).with_priority(1))
            })
            .when(self.sidebar.menu_open, |sidebar| {
                sidebar.child(deferred(self.render_filter_menu(true, cx)).with_priority(1))
            })
            .when(self.sidebar.switch_hovered, |sidebar| {
                sidebar.child(super::filter::switch_tip("Show projects"))
            })
            .children(
                t.hovered
                    .filter(|_| !switcher_open && !self.sidebar.menu_open)
                    .and_then(|(thread, ix)| self.render_thread_popover(thread, ix, cx)),
            )
            .test_support()
    }

    /// The search field, Filter options, Add project, New thread and the
    /// switch back to projects.
    fn render_threads_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.sidebar.threads.query.is_empty();
        let menu_open = self.sidebar.menu_open;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(4.))
            .pt(scale::row(12.))
            .px(ui_px(8.))
            .pb(scale::row(6.))
            .child(
                div()
                    .id("threads-search")
                    .key_context("ThreadsSearch")
                    .flex_1()
                    .min_w_0()
                    .on_action(cx.listener(|this, _: &ThreadsDown, window, cx| {
                        this.move_into_list(1, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &ThreadsUp, window, cx| {
                        this.move_into_list(-1, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &ThreadsOpen, window, cx| {
                        this.open_thread_cursor(window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &ThreadsClear, window, cx| {
                        this.clear_thread_search(window, cx)
                    }))
                    .child(
                        Input::new(&self.sidebar.threads.search)
                            .aria_label("Search threads")
                            .h(scale::row(28.))
                            .text_size(scale::text(12.))
                            .prefix(
                                Icon::new(IconName::Search)
                                    .size(ui_px(13.))
                                    .text_color(color("text-tertiary")),
                            )
                            .when(searching, |input| {
                                input.suffix(
                                    div()
                                        .id("threads-search-clear")
                                        .flex()
                                        .flex_none()
                                        .items_center()
                                        .justify_center()
                                        .size(ui_px(16.))
                                        .rounded(ui_px(4.))
                                        .cursor_pointer()
                                        .text_color(color("text-tertiary"))
                                        .hover(|b| b.text_color(color("text-primary")))
                                        .aria_label("Clear search")
                                        .child(Icon::new(IconName::Close).size(ui_px(10.)))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.clear_thread_search(window, cx);
                                        }))
                                        .test_support(),
                                )
                            }),
                    )
                    .test_support(),
            )
            .child(
                icon_button(
                    "sidebar-filter-options",
                    FUNNEL_SVG,
                    "Filter options",
                    menu_open,
                )
                .aria_expanded(menu_open)
                .on_click(cx.listener(|this, click, window, cx| {
                    this.toggle_filter_menu(click, window, cx);
                }))
                .test_support(),
            )
            .child(
                icon_button(
                    "sidebar-projects-add",
                    FOLDER_PLUS_SVG,
                    "Add project",
                    false,
                )
                .on_click(cx.listener(|this, _, window, cx| this.open_add_project(window, cx)))
                .test_support(),
            )
            .child(
                icon_button("sidebar-new-thread", COMPOSE_SVG, "New thread", false)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.new_thread_from_sidebar(window, cx)),
                    )
                    .test_support(),
            )
            .child(switch_button("Show projects", TREE_SVG, cx))
    }

    /// The channel the bar's New thread starts in when none is open: the
    /// picked chip, else the picked project's first channel, else the
    /// first listed thread's.
    pub(super) fn threads_new_thread_channel(&self) -> Option<ChannelId> {
        let t = &self.sidebar.threads;
        let state = self.state();
        t.channel
            .or_else(|| {
                t.project
                    .and_then(|p| state.channels(p).first().map(|c| c.id))
            })
            .or_else(|| {
                t.order
                    .first()
                    .and_then(|thread| state.thread(*thread))
                    .map(|thread| thread.channel)
            })
    }

    fn render_threads_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.sidebar.threads.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        let el = match row {
            TRow::Heading(group, n) => render_group(group, n).into_any_element(),
            TRow::Thread(thread) => self.render_thread_row(thread, ix, cx),
            TRow::Settled { count, open } => render_settled(count, open, cx).into_any_element(),
            TRow::Empty => {
                let text = if self.sidebar.threads.query.trim().is_empty() {
                    "No threads yet."
                } else {
                    "No threads match."
                };
                div()
                    .id("threads-empty")
                    .px(ui_px(10.))
                    .py(scale::row(6.))
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(text)
                    .child(text)
                    .test_support()
                    .into_any_element()
            }
        };
        div()
            .px(ui_px(6.))
            .pb(ui_px(1.))
            .child(el)
            .into_any_element()
    }

    /// A thread: its project and channel and status, its title, then its
    /// branch and changes, or "answer only", and who ran it.
    fn render_thread_row(&self, thread: ThreadId, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state();
        let t = &self.sidebar.threads;
        let Some(stored) = state.thread(thread) else {
            return div().into_any_element();
        };
        let Some(channel) = state.channel(stored.channel) else {
            return div().into_any_element();
        };
        let Some(project) = state.project(channel.project) else {
            return div().into_any_element();
        };
        let open = self.nav.thread == Some(thread);
        let cursor = t.cursor.and_then(|c| t.order.get(c)) == Some(&thread);
        let hovered = t.hovered.is_some_and(|(h, _)| h == thread);
        let place = format!("{} · #{}", project.name, channel.name);
        let query = t.query.trim();
        let range = match_range(&stored.title, query);
        let summary = t.summaries.by_thread.get(&thread);
        let status = self.thread_card_status(thread);
        let runtime = t
            .facts
            .get(&thread)
            .and_then(|f| f.runtime.clone())
            .or_else(|| {
                summary
                    .and_then(|s| s.participants.last())
                    .and_then(|a| state.agent(*a))
                    .map(|a| a.spec.runtime.clone())
            });
        let changes = summary.filter(|s| s.added > 0 || s.removed > 0);
        let branch = stored.worktree.as_ref().map(|w| w.branch.clone());
        let id = thread;
        div()
            .id(SharedString::from(format!("thread-row-{id}")))
            .flex()
            .flex_col()
            .gap(scale::row(2.))
            .px(ui_px(10.))
            .py(scale::row(8.))
            .rounded(ui_px(6.))
            .cursor_pointer()
            // The open thread is filled; the keys' row is outlined.
            .border_1()
            .border_color(if cursor {
                color("accent-blue")
            } else {
                transparent_black()
            })
            .when(open, |row| row.bg(color("surface-300")))
            .when(hovered && !open, |row| row.bg(color("surface-100")))
            .when(!open, |row| row.hover(|row| row.bg(color("surface-100"))))
            .aria_label(stored.title.clone())
            .aria_selected(open || cursor)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(6.))
                    .min_w_0()
                    .text_size(scale::text(11.))
                    .line_height(scale::line(16.))
                    .text_color(color("text-tertiary"))
                    .whitespace_nowrap()
                    .children(t.badges.get(&project.id).map(project_badge))
                    .child(
                        div()
                            .id(SharedString::from(format!("thread-row-place-{id}")))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .aria_label(place.clone())
                            .child(place)
                            .test_support(),
                    )
                    .child(self.render_row_status(thread, status)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(scale::text(13.))
                    .line_height(scale::line(18.))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color("text-primary"))
                    .child(highlighted(&stored.title, range)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(6.))
                    .min_w_0()
                    .text_size(scale::text(11.))
                    .line_height(scale::line(16.))
                    .text_color(color("text-tertiary"))
                    .whitespace_nowrap()
                    .children(branch.clone().map(|b| {
                        div()
                            .id(SharedString::from(format!("thread-row-branch-{id}")))
                            .min_w_0()
                            .truncate()
                            .font_family(code_family())
                            .text_size(scale::mono(11.))
                            .aria_label(b.clone())
                            .child(b)
                            .test_support()
                    }))
                    .children(
                        changes.map(|s| {
                            diff_counts(format!("thread-row-diff-{id}"), s.added, s.removed)
                        }),
                    )
                    .when(
                        branch.is_none()
                            && changes.is_none()
                            && matches!(
                                status,
                                CardStatus::Done | CardStatus::Stopped | CardStatus::Failed
                            ),
                        |l3| {
                            l3.child(
                                div()
                                    .id(SharedString::from(format!("thread-row-answer-{id}")))
                                    .aria_label("answer only")
                                    .child("answer only")
                                    .test_support(),
                            )
                        },
                    )
                    .child(div().flex_1())
                    .children(runtime.map(|r| provider_icon(&r, 12.))),
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.thread_row_hovered(thread, ix, *hovered, cx);
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.sidebar.threads.cursor = None;
                this.sidebar.threads.hovered = None;
                this.open_place(Place::Thread(thread), window, cx);
            }))
            .test_support()
            .into_any_element()
    }

    /// The row's status on the right: an icon and its word, or a
    /// working run's time.
    fn render_row_status(&self, thread: ThreadId, status: CardStatus) -> impl IntoElement {
        let id = SharedString::from(format!("thread-row-status-{thread}"));
        let (svg, token, size): (&'static [u8], &str, f32) = match status {
            CardStatus::NeedsYou => (ALERT_SVG, "warning", 11.),
            CardStatus::Working => (ARC_SVG, "accent-blue", 11.),
            CardStatus::Done => (DONE_SVG, "success", 11.),
            CardStatus::Stopped => (STOP_SVG, "text-secondary", 9.),
            CardStatus::Failed => (FAILED_SVG, "danger", 11.),
            CardStatus::Queued | CardStatus::Idle => {
                let word = if status == CardStatus::Queued {
                    "Queued"
                } else {
                    ""
                };
                return div()
                    .id(id)
                    .flex_none()
                    .text_color(color("text-tertiary"))
                    .aria_label(word)
                    .child(word)
                    .test_support()
                    .into_any_element();
            }
        };
        let text = if status == CardStatus::Working {
            let since = self
                .sidebar
                .threads
                .facts
                .get(&thread)
                .and_then(|f| f.working_since)
                .unwrap_or_else(now_ms);
            div()
                .font_family(code_family())
                .text_size(scale::mono(11.))
                .child(elapsed(now_ms() - since))
        } else {
            div().child(status.word())
        };
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(4.))
            .text_color(color(token))
            .aria_label(status.word())
            .child(Icon::default().data(svg).size(ui_px(size)))
            .child(text)
            .test_support()
            .into_any_element()
    }

    /// Picks a channel chip, or drops it when it is the one picked.
    pub(super) fn pick_chip(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        let t = &mut self.sidebar.threads;
        t.channel = if t.channel == Some(channel) {
            None
        } else {
            Some(channel)
        };
        t.cursor = None;
        t.list.scroll_to(ListOffset::default());
        cx.notify();
    }

    /// The picked project's channels as chips above the list.
    fn render_channel_chips(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let project = self.sidebar.threads.project?;
        let picked = self.sidebar.threads.channel;
        let chips: Vec<AnyElement> = self
            .state()
            .channels(project)
            .into_iter()
            .map(|c| {
                let id = c.id;
                let on = picked == Some(id);
                let name = format!("#{}", c.name);
                div()
                    .id(SharedString::from(format!("threads-chip-{id}")))
                    .flex_none()
                    .px(ui_px(6.))
                    .rounded(ui_px(4.))
                    .cursor_pointer()
                    .font_family(code_family())
                    .text_size(scale::mono(11.))
                    .line_height(scale::line(16.))
                    // The board's chip; the picked one reads brighter.
                    .bg(color(if on { "border-200" } else { "surface-300" }))
                    .text_color(color(if on { "text-primary" } else { "text-secondary" }))
                    .hover(|c| c.text_color(color("text-primary")))
                    .aria_label(name.clone())
                    .aria_selected(on)
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_chip(id, cx)))
                    .test_support()
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .id("threads-chips")
                .flex()
                .flex_wrap()
                .flex_none()
                .gap(ui_px(4.))
                .px(ui_px(8.))
                .pb(scale::row(6.))
                .children(chips)
                .test_support(),
        )
    }
}

/// A project's two-letter badge in its tint, 16 px at the default text
/// size and growing with it.
pub(super) fn project_badge(badge: &Badge) -> Div {
    let (bg, fg) = badge_tint(badge.tint);
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(scale::text(16.))
        .rounded(ui_px(4.))
        .bg(bg)
        .text_color(fg)
        .text_size(scale::text(9.))
        .font_weight(weight(FontWeight::SEMIBOLD))
        .child(badge.letters.clone())
}

/// `+88 −51`, in their colours; a side that is 0 is left out unless
/// both are.
fn diff_counts(id: String, added: u32, removed: u32) -> impl IntoElement {
    let green = HighlightStyle {
        color: Some(color("success")),
        ..Default::default()
    };
    let red = HighlightStyle {
        color: Some(color("danger")),
        ..Default::default()
    };
    let plus = format!("+{added}");
    let minus = format!("−{removed}");
    let (text, marks) = match (added, removed) {
        (_, 0) => (plus.clone(), vec![(0..plus.len(), green)]),
        (0, _) => (minus.clone(), vec![(0..minus.len(), red)]),
        _ => {
            let text = format!("{plus} {minus}");
            let start = plus.len() + 1;
            let marks = vec![(0..plus.len(), green), (start..text.len(), red)];
            (text, marks)
        }
    };
    div()
        .id(SharedString::from(id))
        .flex_none()
        .font_family(code_family())
        .text_size(scale::mono(11.))
        .aria_label(text.clone())
        .child(StyledText::new(text).with_highlights(marks))
        .test_support()
}

/// "Needs you · 1", with its dot, or "Recent · 4".
fn render_group(group: Group, n: usize) -> impl IntoElement {
    let text = format!("{} · {n}", group.word());
    let dot = match group {
        Group::NeedsYou => Some("warning"),
        Group::Working => Some("accent-blue"),
        Group::Recent => None,
    };
    div()
        .id(SharedString::from(format!("threads-group-{}", group.key())))
        .flex()
        .items_center()
        .gap(ui_px(6.))
        .h(scale::row(28.))
        .px(ui_px(10.))
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-tertiary"))
        .aria_label(text.clone())
        .children(dot.map(super::super::common::dot))
        .child(text)
        .test_support()
}

/// "Settled · {n}", which opens and closes the quiet threads.
fn render_settled(count: usize, open: bool, cx: &mut Context<Workspace>) -> impl IntoElement {
    let text = format!("Settled · {count}");
    div()
        .id("threads-settled")
        .flex()
        .items_center()
        .gap(ui_px(6.))
        .h(scale::row(28.))
        .mt(scale::row(4.))
        .px(ui_px(10.))
        .rounded(ui_px(6.))
        .cursor_pointer()
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-tertiary"))
        .hover(|row| row.bg(color("surface-200")))
        .aria_label(text.clone())
        .aria_expanded(open)
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(ui_px(12.)),
        )
        .child(text)
        .on_click(cx.listener(|this, _, _, cx| {
            let t = &mut this.sidebar.threads;
            if t.query.trim().is_empty() {
                t.settled_open = !t.settled_open;
            } else {
                t.settled_found_open = !t.settled_found_open;
            }
            cx.notify();
        }))
        .test_support()
}

#[cfg(test)]
mod tests {
    use crew_core::ids::{AgentId, ProjectId, ThreadId};
    use crew_core::store::ApprovalKind;

    use super::{ThreadGroups, ThreadInfo, badges, group_threads, span, wants};
    use crate::model::ThreadStatus;
    use crate::ui::sidebar::projects::ACTIVE_WITHIN_MS;

    const NOW: i64 = 100 * ACTIVE_WITHIN_MS;
    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn info(status: ThreadStatus, days_ago: i64) -> ThreadInfo {
        ThreadInfo {
            id: ThreadId::new(),
            status,
            activity: NOW - days_ago * DAY,
        }
    }

    #[test]
    fn threads_group_by_what_they_need_then_by_how_recent() {
        let asks = info(ThreadStatus::NeedsYou, 30);
        let works = info(ThreadStatus::Working, 1);
        let waits = info(ThreadStatus::Queued, 0);
        let fresh = info(ThreadStatus::Idle, 0);
        let week = info(ThreadStatus::Idle, 7);
        let old = info(ThreadStatus::Idle, 8);
        let groups = group_threads(
            vec![
                old.clone(),
                works.clone(),
                week.clone(),
                asks.clone(),
                fresh.clone(),
                waits.clone(),
            ],
            NOW,
        );
        assert_eq!(
            groups,
            ThreadGroups {
                needs_you: vec![asks.id],
                working: vec![waits.id, works.id],
                recent: vec![fresh.id, week.id],
                settled: vec![old.id],
            }
        );
    }

    #[test]
    fn badges_take_initials_then_letters_and_never_clash_while_they_can() {
        let ids: Vec<ProjectId> = (0..6).map(|_| ProjectId::new()).collect();
        let names = ["crew", "curl-runner", "supa-app", "api", "web", "x"];
        let list: Vec<(ProjectId, &str)> = ids.iter().copied().zip(names).collect();
        let got = badges(&list);
        let of = |i: usize| got[&ids[i]].letters.as_str();
        assert_eq!(
            [of(0), of(1), of(2), of(3), of(4), of(5)],
            ["CW", "CR", "SA", "AP", "WE", "X"]
        );
        // Tints go in turn, in the order the projects were added.
        let tints: Vec<usize> = ids.iter().map(|id| got[id].tint).collect();
        assert_eq!(tints, [0, 1, 2, 3, 4, 0]);
    }

    #[test]
    fn spans_read_in_the_largest_unit() {
        assert_eq!(span(45_000), "45s");
        assert_eq!(span(22 * 60_000 + 5_000), "22m");
        assert_eq!(span(63 * 60_000), "1h 3m");
    }

    #[test]
    fn an_ask_says_what_it_wants() {
        let name = |_: AgentId| "Scout".to_string();
        let bash = ApprovalKind::Permission {
            tool: "Bash".into(),
            input: serde_json::json!({}),
            tool_use_id: None,
        };
        assert_eq!(wants(&bash, name), "wants to run a command");
        let handoff = ApprovalKind::Handoff {
            from: AgentId::new(),
            to: AgentId::new(),
            brief: String::new(),
            paused: false,
        };
        assert_eq!(wants(&handoff, name), "wants to bring in Scout");
    }
}
