//! The sidebar's Threads form: every project's threads in one list,
//! grouped Needs you, Working and Recent, with the quiet ones under
//! Settled. Rows sit in a virtual list, so many threads stay smooth.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crew_core::event::RunStatus;
use crew_core::ids::{ChannelId, ProjectId, ThreadId};
use crew_core::store::{ApprovalKind, StoredApproval};
use crew_proto::{Request, SearchResults, ThreadSummary, WorktreeState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::channel_view::{CardStatus, card_status, retry_after};
use super::super::common::{ago, elapsed, now_ms, provider_icon, runtime_chip};
use super::super::palette::{highlighted, match_range};
use super::super::settings::Page;
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
    ]
);

/// How long a change waits before the summaries are asked for again.
const REFETCH_AFTER: Duration = Duration::from_millis(500);
/// Asks again if crewd never says counts it was working out are done.
const COUNTING_FALLBACK: Duration = Duration::from_secs(3);
/// How long typing must pause before crewd searches messages.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
/// Most threads a message search brings.
const SEARCH_LIMIT: u32 = 200;
/// How long the popover stays once the pointer leaves its row.
const POPOVER_LINGER: Duration = Duration::from_millis(150);
/// The popover's width, from the board.
const POPOVER_WIDTH: f32 = 300.;

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
/// The footer's Settings.
const GEAR_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="8" cy="8" r="2"/><path d="M8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M3.4 12.6l1.4-1.4M11.2 4.8l1.4-1.4"/></svg>"#;
/// The footer's Running now.
const PULSE_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M1.5 8h3l2-5 3 10 2-5h3"/></svg>"#;
/// The footer's Agents.
const PEOPLE_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="6" cy="6" r="2.5"/><path d="M1.5 14c0-2.5 2-4 4.5-4s4.5 1.5 4.5 4"/><path d="M11 4a2 2 0 1 1 0 4M12.5 10.5c1.3.5 2 1.6 2 3.5" stroke-linecap="round"/></svg>"#;

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

/// Each project's two-letter badge: the first letters of its first two
/// words, else its first two letters, else its first and last, else its
/// first and another, so no two badges are the same while that can be.
/// Names of more than one word go first.
pub(crate) fn badges(projects: &[(ProjectId, &str)]) -> HashMap<ProjectId, String> {
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
        out.insert(*id, pick);
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

/// A badge's colours, the same for a project every time.
pub(crate) fn badge_tint(project: ProjectId) -> (Hsla, Hsla) {
    let n = project
        .0
        .as_bytes()
        .iter()
        .map(|b| u32::from(*b))
        .sum::<u32>()
        % 4;
    match n {
        0 => (color("info-subtle"), color("accent-blue")),
        1 => (color("success-subtle"), color("success")),
        2 => (color("accent-purple").opacity(0.15), color("accent-purple")),
        _ => (color("surface-300"), color("text-primary")),
    }
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

fn plural(n: impl Into<u64>, one: &str, many: &str) -> String {
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
    pub(super) badges: HashMap<ProjectId, String>,
    pub(crate) settled_open: bool,
    /// The project the switcher picked, if one.
    pub(crate) project: Option<ProjectId>,
    /// The channel chip picked, if one.
    pub(crate) channel: Option<ChannelId>,
    pub(super) switcher: super::switcher::Switcher,
    /// What crewd last said about each thread.
    pub(super) summaries: HashMap<ThreadId, ThreadSummary>,
    /// Moves whenever `summaries` changes.
    summaries_gen: u64,
    fetch: Option<Task<()>>,
    refetch: Option<Task<()>>,
    fallback: Option<Task<()>>,
    retry: Option<Task<()>>,
    failures: u32,
    loaded: bool,
    /// Something changed since the summaries were asked for.
    dirty: bool,
    /// Threads whose messages hold the query, as crewd last said.
    remote: Option<(String, HashSet<ThreadId>)>,
    remote_gen: u64,
    search_task: Option<Task<()>>,
    /// The thread whose popover shows, and its row.
    pub(crate) hovered: Option<(ThreadId, usize)>,
    popover_hovered: bool,
    linger: Option<Task<()>>,
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
            project: None,
            channel: None,
            switcher: super::switcher::Switcher::new(window, cx),
            summaries: HashMap::new(),
            summaries_gen: 0,
            fetch: None,
            refetch: None,
            fallback: None,
            retry: None,
            failures: 0,
            loaded: false,
            dirty: false,
            remote: None,
            remote_gen: 0,
            search_task: None,
            hovered: None,
            popover_hovered: false,
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
        if self.dirty && self.fetch.is_none() {
            self.loaded = false;
            self.refetch = None;
        }
    }
}

/// The mode the sidebar is in.
pub(crate) fn sidebar_mode(cx: &App) -> SidebarMode {
    scale::appearance(cx).sidebar_mode
}

impl Workspace {
    /// Switches the sidebar to the other form, and saves that.
    pub(super) fn switch_sidebar(&mut self, cx: &mut Context<Self>) {
        let mut a = scale::appearance(cx);
        a.sidebar_mode = match a.sidebar_mode {
            SidebarMode::Projects => SidebarMode::Threads,
            SidebarMode::Threads => SidebarMode::Projects,
        };
        scale::set_appearance(a, cx);
        cx.notify();
    }

    fn threads_now(&self) -> i64 {
        #[cfg(test)]
        if let Some(now) = self.sidebar.threads.now {
            return now;
        }
        now_ms()
    }

    /// Notes changes crewd reported, so the summaries are asked for
    /// again.
    pub(crate) fn threads_touched(&mut self, touched: bool) {
        if touched {
            self.sidebar.threads.dirty = true;
        }
    }

    /// Asks crewd for every thread's summary when the threads sidebar
    /// shows first, and again [`REFETCH_AFTER`] a change.
    pub(crate) fn sync_thread_summaries(&mut self, cx: &mut Context<Self>) {
        if sidebar_mode(cx) != SidebarMode::Threads {
            return;
        }
        let t = &mut self.sidebar.threads;
        if t.fetch.is_some() || t.retry.is_some() {
            return;
        }
        if !t.loaded {
            self.fetch_thread_summaries(cx);
        } else if t.dirty && t.refetch.is_none() {
            t.refetch = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REFETCH_AFTER).await;
                this.update(cx, |this, cx| {
                    this.sidebar.threads.refetch = None;
                    if this.sidebar.threads.fetch.is_none() {
                        this.fetch_thread_summaries(cx);
                    }
                })
                .ok();
            }));
        }
    }

    fn fetch_thread_summaries(&mut self, cx: &mut Context<Self>) {
        let t = &mut self.sidebar.threads;
        t.dirty = false;
        t.refetch = None;
        t.fallback = None;
        let answer = self
            .client
            .request(Request::ThreadSummaries { project: None });
        self.sidebar.threads.fetch = Some(cx.spawn(async move |this, cx| {
            let got = answer.await.and_then(|v| {
                serde_json::from_value::<Vec<ThreadSummary>>(v).map_err(|e| e.to_string())
            });
            this.update(cx, |this, cx| {
                let t = &mut this.sidebar.threads;
                t.fetch = None;
                match got {
                    Ok(summaries) => {
                        t.loaded = true;
                        t.failures = 0;
                        let counting = summaries.iter().any(|s| s.counting);
                        t.summaries = summaries.into_iter().map(|s| (s.thread, s)).collect();
                        t.summaries_gen += 1;
                        if counting {
                            t.fallback = Some(cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(COUNTING_FALLBACK).await;
                                this.update(cx, |this, cx| {
                                    this.sidebar.threads.fallback = None;
                                    this.sidebar.threads.dirty = true;
                                    this.sync_thread_summaries(cx);
                                })
                                .ok();
                            }));
                        }
                    }
                    Err(e) => {
                        eprintln!("crew: couldn't load the threads: {e}");
                        t.failures += 1;
                        let wait = retry_after(t.failures);
                        t.retry = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(wait).await;
                            this.update(cx, |this, cx| {
                                this.sidebar.threads.retry = None;
                                this.sidebar.threads.loaded = false;
                                this.sync_thread_summaries(cx);
                            })
                            .ok();
                        }));
                    }
                }
                this.sync_thread_summaries(cx);
                cx.notify();
            })
            .ok();
        }));
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
            summaries: self.sidebar.threads.summaries_gen,
            query: self.sidebar.threads.query.trim().to_lowercase(),
            remote: self.sidebar.threads.remote_gen,
            project: self.sidebar.threads.project,
            channel: self.sidebar.threads.channel,
            show: self.sidebar.prefs.show,
            settled_open: self.sidebar.threads.settled_open,
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
        let settled_open = self.sidebar.threads.settled_open || searching;
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
        let shown: HashSet<ProjectId> = shown
            .pinned
            .into_iter()
            .chain(shown.active)
            .chain(shown.other)
            .collect();
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

    /// ↵ in the search: opens the highlighted thread. Typing goes on in
    /// the search.
    fn open_thread_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t = &self.sidebar.threads;
        let Some(thread) = t.cursor.and_then(|c| t.order.get(c)).copied() else {
            return;
        };
        self.open_place(Place::Thread(thread), window, cx);
        self.sidebar
            .threads
            .search
            .update(cx, |input, cx| input.focus(window, cx));
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
                    .flex_1()
                    .min_h_0()
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
                    .on_action(
                        cx.listener(|this, _: &ThreadsDown, _, cx| this.move_thread_cursor(1, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &ThreadsUp, _, cx| this.move_thread_cursor(-1, cx)),
                    )
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
        let summary = t.summaries.get(&thread);
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
            .when(open || cursor, |row| row.bg(color("surface-300")))
            .when(hovered && !open && !cursor, |row| {
                row.bg(color("surface-100"))
            })
            .hover(|row| row.bg(color("surface-100")))
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
                    .child(project_badge(
                        project.id,
                        t.badges.get(&project.id).cloned().unwrap_or_default(),
                    ))
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
                        branch.is_none() && changes.is_none() && status != CardStatus::Idle,
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

    fn thread_row_hovered(
        &mut self,
        thread: ThreadId,
        ix: usize,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let t = &mut self.sidebar.threads;
        if hovered {
            t.linger = None;
            if t.hovered != Some((thread, ix)) {
                t.hovered = Some((thread, ix));
                cx.notify();
            }
            return;
        }
        if t.hovered.is_some_and(|(h, _)| h == thread) {
            self.close_popover_later(cx);
        }
    }

    /// Closes the popover after [`POPOVER_LINGER`], unless the pointer
    /// went into it or onto another row.
    fn close_popover_later(&mut self, cx: &mut Context<Self>) {
        let shown = self.sidebar.threads.hovered;
        self.sidebar.threads.linger = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(POPOVER_LINGER).await;
            this.update(cx, |this, cx| {
                let t = &mut this.sidebar.threads;
                t.linger = None;
                if !t.popover_hovered && t.hovered == shown {
                    t.hovered = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The hovered thread at a glance, beside its row.
    fn render_thread_popover(
        &self,
        thread: ThreadId,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.state();
        let t = &self.sidebar.threads;
        let stored = state.thread(thread)?;
        let channel = state.channel(stored.channel)?;
        let project = state.project(channel.project)?;
        let row = t.list.bounds_for_item(ix)?;
        let summary = t.summaries.get(&thread);
        let status = self.thread_card_status(thread);
        let now = now_ms();
        let name_of = |a| {
            state
                .agent(a)
                .map_or_else(|| "An agent".to_string(), |a| a.spec.name.clone())
        };
        let (line, token) = match status {
            CardStatus::NeedsYou => {
                let ask: Option<&StoredApproval> = state.pending_approvals(thread).last().copied();
                let text = ask.map_or_else(
                    || "Needs you".to_string(),
                    |a| {
                        let who = state
                            .run(a.run)
                            .map_or_else(|| "An agent".to_string(), |r| name_of(r.agent));
                        format!("Needs you · {who} {}", wants(&a.kind, name_of))
                    },
                );
                (text, "warning")
            }
            CardStatus::Working => {
                let since = t
                    .facts
                    .get(&thread)
                    .and_then(|f| f.working_since)
                    .unwrap_or(now);
                let who: Vec<String> = state
                    .live_runs(thread)
                    .into_iter()
                    .filter(|r| r.status == RunStatus::Working)
                    .map(|r| name_of(r.agent))
                    .collect();
                (
                    format!("Working · {} · {}", who.join(", "), elapsed(now - since)),
                    "accent-blue",
                )
            }
            CardStatus::Done => ("Done".into(), "success"),
            CardStatus::Stopped => ("Stopped".into(), "text-secondary"),
            CardStatus::Failed => ("Failed".into(), "danger"),
            CardStatus::Queued => ("Queued".into(), "text-tertiary"),
            CardStatus::Idle => ("No runs yet".into(), "text-tertiary"),
        };
        let files = summary.map_or(0, |s| s.files);
        let (added, removed) = summary.map_or((0, 0), |s| (s.added, s.removed));
        let tree = summary.map_or(WorktreeState::None, |s| s.worktree);
        let changes_text = if files == 0 && added == 0 && removed == 0 {
            "No changes".to_string()
        } else {
            let state_word = match tree {
                WorktreeState::Uncommitted { .. } => Some("uncommitted"),
                WorktreeState::Committed => Some("committed"),
                WorktreeState::Clean | WorktreeState::None => None,
            };
            let mut text = format!("{} · +{added} −{removed}", plural(files, "file", "files"));
            if let Some(word) = state_word {
                text.push_str(&format!(" · {word}"));
            }
            text
        };
        let agents: Vec<(String, String)> = summary
            .map(|s| s.participants.clone())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| state.agent(a))
            .map(|a| (a.spec.name.clone(), runtime_chip(&a.spec)))
            .collect();
        let activity = summary.map_or_else(String::new, |s| {
            format!(
                "{} · {} · {}",
                plural(s.messages, "message", "messages"),
                plural(s.runs.len() as u64, "run", "runs"),
                span(s.agent_ms)
            )
        });
        let last = t
            .facts
            .get(&thread)
            .map(|f| f.activity)
            .unwrap_or(stored.created_at)
            .max(summary.map_or(0, |s| s.last_activity_at));
        let reviewable = stored.worktree.is_some() && files > 0;
        let x = self.sidebar_width() + ui_px(8.);
        let kv = |key: &'static str, id: &'static str, label: String, value: AnyElement| {
            div()
                .flex()
                .gap(ui_px(12.))
                .px(ui_px(12.))
                .py(scale::row(4.))
                .text_size(scale::text(12.))
                .line_height(scale::line(18.))
                .child(
                    div()
                        .w(ui_px(72.))
                        .flex_none()
                        .text_color(color("text-tertiary"))
                        .child(key),
                )
                .child(
                    div()
                        .id(id)
                        .flex_1()
                        .min_w_0()
                        .text_color(color("text-primary"))
                        .aria_label(label)
                        .child(value)
                        .test_support(),
                )
        };
        let place = format!("{} · #{}", project.name, channel.name);
        let branch = stored.worktree.as_ref().map(|w| w.branch.clone());
        let pop = div()
            .id("thread-popover")
            .w(ui_px(POPOVER_WIDTH))
            .bg(color("surface-raised"))
            .border_1()
            .border_color(color("border-200"))
            .rounded(ui_px(8.))
            .shadow_lg()
            .occlude()
            .aria_label(stored.title.clone())
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.sidebar.threads.popover_hovered = *hovered;
                if *hovered {
                    this.sidebar.threads.linger = None;
                } else {
                    this.close_popover_later(cx);
                }
            }))
            .child(
                div()
                    .px(ui_px(12.))
                    .pt(scale::row(12.))
                    .pb(scale::row(8.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .child(
                        div()
                            .font_weight(weight(FontWeight::SEMIBOLD))
                            .line_height(scale::line(20.))
                            .child(stored.title.clone()),
                    )
                    .child(
                        div()
                            .id("thread-popover-status")
                            .mt(scale::row(2.))
                            .text_size(scale::text(12.))
                            .text_color(color(token))
                            .aria_label(line.clone())
                            .child(line)
                            .test_support(),
                    ),
            )
            .child(
                div()
                    .py(scale::row(8.))
                    .child(kv(
                        "Project",
                        "thread-popover-project",
                        place.clone(),
                        place.into_any_element(),
                    ))
                    .children(branch.map(|b| {
                        kv(
                            "Branch",
                            "thread-popover-branch",
                            b.clone(),
                            div()
                                .font_family(code_family())
                                .text_size(scale::mono(11.))
                                .truncate()
                                .child(b)
                                .into_any_element(),
                        )
                    }))
                    .child(kv(
                        "Changes",
                        "thread-popover-changes",
                        changes_text.clone(),
                        changes_value(files, added, removed, tree).into_any_element(),
                    ))
                    .when(!agents.is_empty(), |list| {
                        let label = agents
                            .iter()
                            .map(|(n, c)| format!("{n} {c}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        list.child(kv(
                            "Agents",
                            "thread-popover-agents",
                            label,
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(ui_px(6.))
                                .children(agents.into_iter().map(|(name, chip)| {
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(ui_px(4.))
                                        .child(name)
                                        .child(super::super::common::chip(chip))
                                }))
                                .into_any_element(),
                        ))
                    })
                    .when(!activity.is_empty(), |list| {
                        list.child(kv(
                            "Activity",
                            "thread-popover-activity",
                            activity.clone(),
                            activity.clone().into_any_element(),
                        ))
                    })
                    .child({
                        let last = ago(last, now);
                        kv(
                            "Last",
                            "thread-popover-last",
                            last.clone(),
                            last.into_any_element(),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(ui_px(6.))
                    .px(ui_px(12.))
                    .py(scale::row(8.))
                    .border_t_1()
                    .border_color(color("border-100"))
                    .child(
                        small_button("thread-popover-open", "Open", false)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.sidebar.threads.hovered = None;
                                this.open_place(Place::Thread(thread), window, cx);
                            }))
                            .test_support(),
                    )
                    .when(reviewable, |bar| {
                        bar.child(
                            small_button("thread-popover-review", "Review changes", true)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.sidebar.threads.hovered = None;
                                    this.leave_pages(cx);
                                    this.open_review(thread, window, cx);
                                }))
                                .test_support(),
                        )
                    }),
            )
            .test_support();
        Some(
            deferred(
                anchored()
                    .position(point(x, row.origin.y))
                    .snap_to_window_with_margin(ui_px(8.))
                    .child(pop),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// Settings, Running now with its count, Agents and the palette's
    /// shortcut.
    fn render_threads_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.state().working_runs().len();
        let label = format!("Running now, {running} running");
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(2.))
            .p(ui_px(8.))
            .border_t_1()
            .border_color(color("border-100"))
            .child(
                footer_button("threads-footer-settings", GEAR_SVG, "Settings")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(Page::General, window, cx)
                    }))
                    .test_support(),
            )
            .child(
                div()
                    .id("threads-footer-running")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(ui_px(6.))
                    .h(ui_px(28.))
                    .px(ui_px(8.))
                    .rounded(ui_px(6.))
                    .cursor_pointer()
                    .text_size(scale::text(12.))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color("text-secondary"))
                    .hover(|b| b.bg(color("surface-300")))
                    .aria_label(label)
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Running now").build(window, cx)
                    })
                    .child(Icon::default().data(PULSE_SVG).size(ui_px(14.)))
                    .child(
                        div()
                            .text_color(color(if running > 0 {
                                "accent-blue"
                            } else {
                                "text-tertiary"
                            }))
                            .child(running.to_string()),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.show_running(window, cx)))
                    .test_support(),
            )
            .child(
                footer_button("threads-footer-agents", PEOPLE_SVG, "Agents")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(Page::Agents, window, cx)
                    }))
                    .test_support(),
            )
            .child(
                div()
                    .ml_auto()
                    .min_w_0()
                    .overflow_hidden()
                    .pr(ui_px(4.))
                    .flex()
                    .gap(ui_px(4.))
                    .whitespace_nowrap()
                    .text_size(scale::text(11.))
                    .text_color(color("text-tertiary"))
                    .child(div().flex_none().font_family(code_family()).child("⌘K"))
                    .child(div().min_w_0().truncate().child("projects, channels")),
            )
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
                    .border_1()
                    .cursor_pointer()
                    .font_family(code_family())
                    .text_size(scale::mono(11.))
                    .line_height(scale::line(16.))
                    .border_color(color(if on { "border-200" } else { "border-100" }))
                    .bg(color(if on { "surface-300" } else { "bg-100" }))
                    .text_color(color(if on { "text-primary" } else { "text-secondary" }))
                    .hover(|c| c.bg(color("surface-300")))
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

/// A project's two-letter badge in its tint.
pub(super) fn project_badge(project: ProjectId, letters: String) -> Div {
    let (bg, fg) = badge_tint(project);
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(ui_px(16.))
        .rounded(ui_px(4.))
        .bg(bg)
        .text_color(fg)
        .text_size(scale::text(9.))
        .font_weight(weight(FontWeight::SEMIBOLD))
        .child(letters)
}

/// `+88 −51`, in their colours; a side that is 0 is left out unless
/// both are.
fn diff_counts(id: String, added: u32, removed: u32) -> impl IntoElement {
    let label = match (added, removed) {
        (a, 0) => format!("+{a}"),
        (0, r) => format!("−{r}"),
        (a, r) => format!("+{a} −{r}"),
    };
    div()
        .id(SharedString::from(id))
        .flex()
        .flex_none()
        .gap(ui_px(4.))
        .font_family(code_family())
        .text_size(scale::mono(11.))
        .aria_label(label)
        .when(added > 0 || removed == 0, |d| {
            d.child(
                div()
                    .text_color(color("success"))
                    .child(format!("+{added}")),
            )
        })
        .when(removed > 0, |d| {
            d.child(
                div()
                    .text_color(color("danger"))
                    .child(format!("−{removed}")),
            )
        })
        .test_support()
}

/// "4 files · +88 −51 · uncommitted", the counts in their colours.
fn changes_value(files: u32, added: u32, removed: u32, tree: WorktreeState) -> Div {
    if files == 0 && added == 0 && removed == 0 {
        return div().child("No changes");
    }
    let word = match tree {
        WorktreeState::Uncommitted { .. } => Some("uncommitted"),
        WorktreeState::Committed => Some("committed"),
        WorktreeState::Clean | WorktreeState::None => None,
    };
    div()
        .flex()
        .flex_wrap()
        .gap(ui_px(4.))
        .child(format!("{} ·", plural(files, "file", "files")))
        .child(
            div()
                .font_family(code_family())
                .text_color(color("success"))
                .child(format!("+{added}")),
        )
        .child(
            div()
                .font_family(code_family())
                .text_color(color("danger"))
                .child(format!("−{removed}")),
        )
        .children(word.map(|w| format!("· {w}")))
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
            t.settled_open = !t.settled_open;
            cx.notify();
        }))
        .test_support()
}

/// A 28 px icon button of the footer.
fn footer_button(id: &'static str, svg: &'static [u8], label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(ui_px(28.))
        .rounded(ui_px(6.))
        .cursor_pointer()
        .text_color(color("text-secondary"))
        .hover(|b| b.bg(color("surface-300")).text_color(color("text-primary")))
        .aria_label(label)
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(label).build(window, cx)
        })
        .child(Icon::default().data(svg).size(ui_px(15.)))
}

/// The popover's 24 px button, outlined or ghost.
fn small_button(id: &'static str, label: &'static str, ghost: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(ui_px(24.))
        .px(ui_px(8.))
        .rounded(ui_px(6.))
        .border_1()
        .cursor_pointer()
        .whitespace_nowrap()
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .when(ghost, |b| {
            b.border_color(transparent_black())
                .text_color(color("text-secondary"))
        })
        .when(!ghost, |b| {
            b.border_color(color("border-200"))
                .bg(color("bg-100"))
                .text_color(color("text-primary"))
        })
        .hover(|b| b.bg(color("surface-300")))
        .aria_label(label)
        .child(label)
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
        let of = |i: usize| got[&ids[i]].as_str();
        assert_eq!(
            [of(0), of(1), of(2), of(3), of(4), of(5)],
            ["CW", "CR", "SA", "AP", "WE", "X"]
        );
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
