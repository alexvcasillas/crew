//! A channel's view: its header and stats, a toolbar to filter, sort and
//! search, and one card per thread, grouped in sections.

mod card;
mod empty;
mod new_thread;
mod stats;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crew_core::event::RunStatus;
use crew_core::ids::{ChannelId, ThreadId};
use crew_proto::{ChannelOverview, Request, ThreadSummary, WorktreeState};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;

use super::composer::ComposerMode;
use super::run_card::{Step, StepState};
use super::workspace::Workspace;
use crate::theme::color;

/// How long the view waits after a change before asking crewd again.
const REFETCH_AFTER: Duration = Duration::from_millis(500);
/// While crewd is still counting changes, how long the view waits for it
/// to say they are done before asking again anyway.
const COUNTING_FALLBACK: Duration = Duration::from_secs(3);
/// "This week" and "Recent": the last 7 days.
pub const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// What a thread card says about its thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CardStatus {
    NeedsYou,
    Working,
    /// Runs wait for a free slot and none works yet.
    Queued,
    Done,
    Stopped,
    Failed,
    Idle,
}

impl CardStatus {
    pub fn word(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Working => "Working",
            Self::Queued => "Queued",
            Self::Done => "Done",
            Self::Stopped => "Stopped",
            Self::Failed => "Failed",
            Self::Idle => "Idle",
        }
    }
}

/// A card's status: an ask waiting on you comes first, then a working
/// run, then one waiting for a slot, then how the latest run ended.
pub fn card_status(
    pending: bool,
    working: bool,
    queued: bool,
    last: Option<&RunStatus>,
) -> CardStatus {
    if pending {
        return CardStatus::NeedsYou;
    }
    if working {
        return CardStatus::Working;
    }
    if queued {
        return CardStatus::Queued;
    }
    match last {
        Some(RunStatus::Done) => CardStatus::Done,
        Some(RunStatus::Stopped | RunStatus::Interrupted) => CardStatus::Stopped,
        Some(RunStatus::Failed) => CardStatus::Failed,
        // Not live any more, and how it ended isn't known yet.
        Some(RunStatus::Queued | RunStatus::Working) | None => CardStatus::Idle,
    }
}

/// The right-click menu's Stop item: `Stop Forge` for one working agent,
/// `Stop all in thread` for more, nothing when none works.
pub fn stop_label(working: &[String]) -> Option<String> {
    match working {
        [] => None,
        [one] => Some(format!("Stop {one}")),
        _ => Some("Stop all in thread".into()),
    }
}

/// How long to wait before asking again after `failures` failed
/// overviews in a row: 2 s, 5 s, 15 s, then 30 s.
pub fn retry_after(failures: u32) -> Duration {
    let secs = match failures {
        0 | 1 => 2,
        2 => 5,
        3 => 15,
        _ => 30,
    };
    Duration::from_secs(secs)
}

/// The toolbar's filters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Working,
    NeedsYou,
    Done,
    Archived,
}

impl Filter {
    pub const EVERY: [Filter; 5] = [
        Self::All,
        Self::Working,
        Self::NeedsYou,
        Self::Done,
        Self::Archived,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Working => "Working",
            Self::NeedsYou => "Needs you",
            Self::Done => "Done",
            Self::Archived => "Archived",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Working => "working",
            Self::NeedsYou => "needs",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }

    fn admits(self, card: &Card) -> bool {
        match self {
            Self::Archived => card.archived,
            _ if card.archived => false,
            Self::All => true,
            Self::Working => card.status == CardStatus::Working,
            Self::NeedsYou => card.status == CardStatus::NeedsYou,
            Self::Done => card.status == CardStatus::Done,
        }
    }
}

/// How cards are ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Latest,
    Created,
    Changes,
}

impl Sort {
    pub const EVERY: [Sort; 3] = [Self::Latest, Self::Created, Self::Changes];

    pub fn label(self) -> &'static str {
        match self {
            Self::Latest => "Latest activity",
            Self::Created => "Created",
            Self::Changes => "Most changes",
        }
    }
}

/// The groups of the All filter, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    NeedsYou,
    Working,
    Recent,
    Older,
}

impl Section {
    pub const EVERY: [Section; 4] = [Self::NeedsYou, Self::Working, Self::Recent, Self::Older];

    pub fn title(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Working => "Working",
            Self::Recent => "Recent",
            Self::Older => "Older",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs",
            Self::Working => "working",
            Self::Recent => "recent",
            Self::Older => "older",
        }
    }

    fn of(card: &Card, now: i64) -> Self {
        match card.status {
            CardStatus::NeedsYou => Self::NeedsYou,
            CardStatus::Working => Self::Working,
            _ if now - card.last_activity <= WEEK_MS => Self::Recent,
            _ => Self::Older,
        }
    }
}

/// One thread as its card shows it, before the live parts.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub thread: ThreadId,
    pub title: String,
    pub created_at: i64,
    pub archived: bool,
    pub status: CardStatus,
    /// Unix ms.
    pub last_activity: i64,
    pub summary: ThreadSummary,
    /// `crew/<slug>`, once the thread has a worktree.
    pub branch: Option<String>,
}

/// One row of the card list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Section {
        section: Section,
        count: usize,
        open: bool,
        first: bool,
    },
    /// Two cards side by side, or one on the left.
    Cards(ThreadId, Option<ThreadId>),
    /// Nothing matches the filter or the search.
    Nothing,
}

/// What the toolbar asks for.
#[derive(Debug, Clone, Default)]
pub struct Shown<'a> {
    pub filter: Filter,
    pub sort: Sort,
    pub query: &'a str,
    pub folded: Option<&'a HashSet<Section>>,
}

/// The rows to draw: the cards `shown` admits, sorted, in sections under
/// All and as a flat list otherwise. Empty sections are left out.
pub fn arrange(cards: &[Card], shown: &Shown<'_>, now: i64) -> Vec<Row> {
    let query = shown.query.trim().to_lowercase();
    let mut picked: Vec<&Card> = cards
        .iter()
        .filter(|c| shown.filter.admits(c))
        .filter(|c| {
            query.is_empty()
                || c.title.to_lowercase().contains(&query)
                || c.summary.excerpt.to_lowercase().contains(&query)
        })
        .collect();
    sort_cards(&mut picked, shown.sort);
    let mut rows = Vec::new();
    if shown.filter == Filter::All {
        for section in Section::EVERY {
            let these: Vec<&Card> = picked
                .iter()
                .copied()
                .filter(|c| Section::of(c, now) == section)
                .collect();
            if these.is_empty() {
                continue;
            }
            let open = !shown.folded.is_some_and(|f| f.contains(&section));
            rows.push(Row::Section {
                section,
                count: these.len(),
                open,
                first: rows.is_empty(),
            });
            if open {
                pairs(&these, &mut rows);
            }
        }
    } else {
        pairs(&picked, &mut rows);
    }
    if rows.is_empty() {
        rows.push(Row::Nothing);
    }
    rows
}

fn pairs(cards: &[&Card], rows: &mut Vec<Row>) {
    for pair in cards.chunks(2) {
        rows.push(Row::Cards(pair[0].thread, pair.get(1).map(|c| c.thread)));
    }
}

fn sort_cards(cards: &mut [&Card], sort: Sort) {
    let tie = |c: &Card| c.thread.0;
    match sort {
        Sort::Latest => cards.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then(tie(a).cmp(&tie(b)))
        }),
        Sort::Created => {
            cards.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(tie(a).cmp(&tie(b))))
        }
        Sort::Changes => cards.sort_by(|a, b| {
            let lines = |c: &Card| u64::from(c.summary.added) + u64::from(c.summary.removed);
            lines(b)
                .cmp(&lines(a))
                .then(b.last_activity.cmp(&a.last_activity))
                .then(tie(a).cmp(&tie(b)))
        }),
    }
}

/// How many cards each filter admits, in [`Filter::EVERY`] order.
pub fn filter_counts(cards: &[Card]) -> [usize; 5] {
    Filter::EVERY.map(|f| cards.iter().filter(|c| f.admits(c)).count())
}

/// `1 message` or `3 messages`.
pub fn count(n: u32, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

/// `1204` reads as `1,204`.
pub fn thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `+57 −19`.
pub fn lines_changed(added: u32, removed: u32) -> String {
    format!("+{} −{}", thousands(added), thousands(removed))
}

/// The counts row: messages, runs, handoffs, files and agent time, or
/// `running {elapsed}` while an agent works.
pub fn counts_line(summary: &ThreadSummary, running: Option<&str>) -> String {
    let runs = u32::try_from(summary.runs.len()).unwrap_or(u32::MAX);
    let mut files = count(summary.files, "file", "files");
    if summary.files > 0 {
        files = format!("{files} {}", lines_changed(summary.added, summary.removed));
    }
    let last = match running {
        Some(elapsed) => format!("running {elapsed}"),
        None => format!(
            "{} agent time",
            super::running_view::usage_time(summary.agent_ms)
        ),
    };
    [
        count(summary.messages, "message", "messages"),
        count(runs, "run", "runs"),
        count(summary.handoffs, "handoff", "handoffs"),
        files,
        last,
    ]
    .join(" · ")
}

/// Where the worktree stands, as the card says it, and its colour token.
pub fn worktree_word(state: WorktreeState, has_branch: bool) -> (String, &'static str) {
    match state {
        WorktreeState::Uncommitted { files } => (format!("{files} uncommitted"), "text-secondary"),
        WorktreeState::Committed => ("committed".into(), "success"),
        WorktreeState::Clean => ("clean".into(), "text-tertiary"),
        WorktreeState::None if has_branch => ("clean".into(), "text-tertiary"),
        WorktreeState::None => ("No worktree changes".into(), "text-tertiary"),
    }
}

/// When a card's thread last moved, at `now` (both Unix ms): `just now`,
/// `2m ago`, `1h ago`, `Yesterday`, then a date.
pub fn card_time(then: i64, now: i64) -> String {
    use chrono::{Datelike as _, Local, TimeZone as _};
    let mins = (now - then).max(0) / 60_000;
    if mins < 1 {
        return "just now".into();
    }
    if mins < 60 {
        return format!("{mins}m ago");
    }
    if mins < 24 * 60 {
        return format!("{}h ago", mins / 60);
    }
    let (Some(then), Some(now)) = (
        Local.timestamp_millis_opt(then).single(),
        Local.timestamp_millis_opt(now).single(),
    ) else {
        return String::new();
    };
    if now.date_naive().pred_opt() == Some(then.date_naive()) {
        return "Yesterday".into();
    }
    if then.year() == now.year() {
        then.format("%b %-d").to_string()
    } else {
        then.format("%b %-d, %Y").to_string()
    }
}

/// What a working card says: `Forge is running cargo test` from a step
/// in progress, else `Forge is working.`
pub fn working_line(agent: &str, step: Option<&Step>) -> String {
    match step {
        Some(step) if step.state == StepState::Running && step.verb.ends_with("ing") => {
            let text = step.text();
            let mut chars = text.chars();
            let first: String = chars
                .next()
                .into_iter()
                .flat_map(char::to_lowercase)
                .collect();
            format!("{agent} is {first}{}", chars.as_str())
        }
        _ => format!("{agent} is working."),
    }
}

/// A run strip's segments as (run index, width): each run's share of
/// `scale` ms on a `track` px wide strip, at least 3 px, with `gap` px
/// between. When they don't fit, every width shrinks alike down to 2 px,
/// then the oldest runs are left out, so the newest always show.
pub fn strip_layout(runs: &[i64], scale: i64, track: f32, gap: f32) -> Vec<(usize, f32)> {
    let scale = scale.max(1) as f32;
    let mut segments: Vec<(usize, f32)> = runs
        .iter()
        .enumerate()
        .map(|(ix, ms)| (ix, (track * (*ms).max(0) as f32 / scale).clamp(3., track)))
        .collect();
    let gaps = |n: usize| gap * n.saturating_sub(1) as f32;
    let total: f32 = segments.iter().map(|(_, w)| w).sum();
    let room = track - gaps(segments.len());
    if total > room {
        let factor = room.max(0.) / total;
        for (_, w) in &mut segments {
            *w = (*w * factor).max(2.);
        }
    }
    let fits =
        |s: &[(usize, f32)]| s.iter().map(|(_, w)| w).sum::<f32>() + gaps(s.len()) <= track + 0.01;
    while segments.len() > 1 && !fits(&segments) {
        segments.remove(0);
    }
    segments
}

/// What a channel's rows were built from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowsKey {
    channel: ChannelId,
    generation: u64,
    filter: Filter,
    sort: Sort,
    query: String,
    folded: Vec<Section>,
    hour: i64,
}

/// What the channel view keeps between redraws.
pub struct ChannelView {
    /// The channel the rest is about.
    pub(super) channel: Option<ChannelId>,
    overview: Option<ChannelOverview>,
    fetch: Option<Task<()>>,
    refetch: Option<Task<()>>,
    /// Asks again if crewd never says counts it was working out are done.
    fallback: Option<Task<()>>,
    /// Why the last overview failed, shown until one loads.
    pub(super) error: Option<String>,
    /// Failed overviews in a row.
    failures: u32,
    /// Asks again after a failure, backing off.
    retry: Option<Task<()>>,
    /// Moves whenever something the cards are built from changes.
    generation: u64,
    /// What the rows were last built from.
    built: Option<RowsKey>,
    /// Something changed in the channel since the last overview.
    dirty: bool,
    /// Whether the view was shown when last synced.
    shown: bool,
    pub(super) filter: Filter,
    pub(super) sort: Sort,
    pub(super) folded: HashSet<Section>,
    pub(super) hovered: Option<ThreadId>,
    /// The card whose overflow menu is open.
    pub(super) menu_open: Option<ThreadId>,
    /// The thread you had open last, highlighted when you come back.
    pub(super) selected: Option<ThreadId>,
    pub(super) search: Entity<InputState>,
    _search_events: Subscription,
    pub(super) list: ListState,
    rows: Vec<Row>,
    cards: HashMap<ThreadId, Card>,
    counts: [usize; 5],
    /// The longest thread's agent time, which fills a run strip.
    scale: i64,
    /// The next new rows show from the top.
    to_top: bool,
}

impl ChannelView {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search threads"));
        let events = cx.subscribe(&search, |this: &mut Workspace, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.channel_view.to_top = true;
                cx.notify();
            }
        });
        Self {
            channel: None,
            overview: None,
            fetch: None,
            refetch: None,
            fallback: None,
            error: None,
            failures: 0,
            retry: None,
            generation: 0,
            built: None,
            dirty: false,
            shown: false,
            filter: Filter::All,
            sort: Sort::Latest,
            folded: HashSet::from([Section::Older]),
            hovered: None,
            menu_open: None,
            selected: None,
            search,
            _search_events: events,
            list: ListState::new(0, ListAlignment::Top, px(600.)),
            rows: Vec::new(),
            cards: HashMap::new(),
            counts: [0; 5],
            scale: 0,
            to_top: false,
        }
    }

    /// Starts over for `channel`: its filter, sort, sections and overview
    /// go back to how a channel opens.
    fn start(&mut self, channel: ChannelId) {
        self.channel = Some(channel);
        self.overview = None;
        self.fetch = None;
        self.refetch = None;
        self.fallback = None;
        self.error = None;
        self.failures = 0;
        self.retry = None;
        self.generation += 1;
        self.dirty = false;
        self.filter = Filter::All;
        self.sort = Sort::Latest;
        self.folded = HashSet::from([Section::Older]);
        self.hovered = None;
        self.menu_open = None;
        self.to_top = true;
    }

    /// Whether `thread`'s card is in the rows drawn now: not hidden by the
    /// filter, the search or a folded section.
    pub(super) fn card_in_view(&self, thread: ThreadId) -> bool {
        self.rows.iter().any(|row| match row {
            Row::Cards(left, right) => *left == thread || *right == Some(thread),
            _ => false,
        })
    }

    /// Where the worktree of `thread`'s card stands, as last counted.
    pub(super) fn card_worktree(&self, thread: ThreadId) -> Option<WorktreeState> {
        self.cards.get(&thread).map(|c| c.summary.worktree)
    }

    pub(super) fn stats(&self) -> crew_proto::ChannelStats {
        self.overview
            .as_ref()
            .map(|o| o.stats.clone())
            .unwrap_or_default()
    }
}

impl Workspace {
    /// The channel whose view is in the main area, if one is.
    pub(super) fn shown_channel(&self) -> Option<ChannelId> {
        let channel = self.nav.channel?;
        let shown = self.nav.thread.is_none()
            && self.terminal.is_none()
            && self.state().channel(channel).is_some();
        shown.then_some(channel)
    }

    /// Shows `channel` from the start, with an empty search.
    pub(super) fn reset_channel_view(
        &mut self,
        channel: ChannelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.channel_view.channel == Some(channel) {
            return;
        }
        self.channel_view.start(channel);
        self.channel_view
            .search
            .update(cx, |search, cx| search.set_value("", window, cx));
    }

    /// Notes changes crewd reported in `threads`, or in the overviews of
    /// `stale` channels, so the overview of the shown channel is asked for
    /// again.
    pub(super) fn channel_touched(
        &mut self,
        threads: &[ThreadId],
        stale: &[ChannelId],
        snapshot: bool,
    ) {
        let Some(channel) = self.channel_view.channel else {
            return;
        };
        let state = self.state();
        let touched = snapshot
            || stale.contains(&channel)
            || threads
                .iter()
                .any(|t| state.thread(*t).is_some_and(|t| t.channel == channel));
        if touched {
            self.channel_view.dirty = true;
            self.channel_view.generation += 1;
        }
    }

    /// Asks crewd for the shown channel's overview when it opens, and
    /// again [`REFETCH_AFTER`] a change.
    pub(super) fn sync_channel(&mut self, cx: &mut Context<Self>) {
        let shown = self.shown_channel();
        let was_shown = std::mem::replace(&mut self.channel_view.shown, shown.is_some());
        let Some(channel) = shown else {
            return;
        };
        let view = &mut self.channel_view;
        if view.channel != Some(channel) {
            view.start(channel);
        }
        if view.fetch.is_some() {
            return;
        }
        // After a failure only the retry timer or a change asks again.
        let first = view.overview.is_none() && view.error.is_none();
        if first || (view.dirty && !was_shown) {
            self.fetch_overview(channel, cx);
        } else if view.dirty && view.refetch.is_none() {
            view.refetch = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REFETCH_AFTER).await;
                this.update(cx, |this, cx| {
                    this.channel_view.refetch = None;
                    if this.channel_view.channel == Some(channel) {
                        this.fetch_overview(channel, cx);
                    }
                })
                .ok();
            }));
        }
    }

    fn fetch_overview(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        let view = &mut self.channel_view;
        view.dirty = false;
        view.refetch = None;
        view.fallback = None;
        let answer = self.client.request(Request::ChannelOverview { channel });
        self.channel_view.fetch = Some(cx.spawn(async move |this, cx| {
            let overview = answer.await.and_then(|v| {
                serde_json::from_value::<ChannelOverview>(v).map_err(|e| e.to_string())
            });
            this.update(cx, |this, cx| {
                this.channel_view.fetch = None;
                match overview {
                    Ok(overview) if this.channel_view.channel == Some(channel) => {
                        let view = &mut this.channel_view;
                        view.error = None;
                        view.failures = 0;
                        view.retry = None;
                        view.generation += 1;
                        // crewd says when counts it is still working out
                        // are done; ask again later in case it never does.
                        if overview.threads.iter().any(|t| t.counting) {
                            this.channel_view.fallback = Some(this.counting_fallback(channel, cx));
                        }
                        this.channel_view.overview = Some(overview);
                    }
                    Ok(_) => {}
                    // The last good overview stays. A change or the retry
                    // timer asks again.
                    Err(e) if this.channel_view.channel == Some(channel) => {
                        let view = &mut this.channel_view;
                        view.failures += 1;
                        let wait = retry_after(view.failures);
                        if view.error.as_deref() != Some(e.as_str()) {
                            eprintln!("crew: couldn't load the channel: {e}");
                        }
                        view.error = Some(e);
                        view.retry = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(wait).await;
                            this.update(cx, |this, cx| {
                                this.channel_view.retry = None;
                                if this.channel_view.channel == Some(channel)
                                    && this.channel_view.fetch.is_none()
                                {
                                    this.fetch_overview(channel, cx);
                                }
                            })
                            .ok();
                        }));
                    }
                    Err(_) => {}
                }
                this.sync_channel(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Marks the overview of `channel` for asking again after
    /// [`COUNTING_FALLBACK`].
    fn counting_fallback(&self, channel: ChannelId, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COUNTING_FALLBACK).await;
            this.update(cx, |this, cx| {
                this.channel_view.fallback = None;
                if this.channel_view.channel == Some(channel) {
                    this.channel_view.dirty = true;
                    this.sync_channel(cx);
                    cx.notify();
                }
            })
            .ok();
        })
    }

    /// Whether the shown channel has a working run, so its cards' timers
    /// need a redraw every second.
    pub(super) fn channel_ticks(&self) -> bool {
        let Some(channel) = self.shown_channel() else {
            return false;
        };
        let state = self.state();
        state.running().iter().any(|r| {
            r.status == RunStatus::Working
                && state.thread(r.thread).is_some_and(|t| t.channel == channel)
        })
    }

    /// Works out the cards and rows to draw, only when what they are built
    /// from changed since the last time. The list keeps its place unless
    /// the toolbar changed.
    pub(super) fn refresh_channel_rows(&mut self, cx: &App) {
        let Some(channel) = self.shown_channel() else {
            return;
        };
        let now = super::common::now_ms();
        let view = &self.channel_view;
        let mut folded: Vec<Section> = view.folded.iter().copied().collect();
        folded.sort_by_key(|s| s.key());
        let key = RowsKey {
            channel,
            generation: view.generation,
            filter: view.filter,
            sort: view.sort,
            query: view.search.read(cx).value().to_string(),
            folded,
            // Recent turns Older as time passes.
            hour: now / (60 * 60 * 1000),
        };
        if view.built.as_ref() == Some(&key) {
            return;
        }
        let state = self.state();
        let summaries: HashMap<ThreadId, &ThreadSummary> = self
            .channel_view
            .overview
            .iter()
            .flat_map(|o| o.threads.iter())
            .map(|s| (s.thread, s))
            .collect();
        // Once per redraw, not once per thread.
        let mut latest: HashMap<ThreadId, &crew_core::store::StoredRun> = HashMap::new();
        for run in state.all_runs() {
            let newer = latest
                .get(&run.thread)
                .is_none_or(|known| (run.started_at, run.id.0) > (known.started_at, known.id.0));
            if newer {
                latest.insert(run.thread, run);
            }
        }
        let (mut working, mut queued) = (HashSet::new(), HashSet::new());
        for run in state.running() {
            match run.status {
                RunStatus::Working => working.insert(run.thread),
                _ => queued.insert(run.thread),
            };
        }
        let pending: HashSet<ThreadId> = state
            .needs_you()
            .iter()
            .map(|n| n.approval.thread)
            .collect();
        let cards: Vec<Card> = state
            .threads_with_archived(channel)
            .into_iter()
            .map(|t| {
                let summary = summaries.get(&t.id).map_or_else(
                    || ThreadSummary {
                        thread: t.id,
                        messages: 0,
                        runs: Vec::new(),
                        handoffs: t.handoffs_used,
                        participants: Vec::new(),
                        excerpt: String::new(),
                        last_activity_at: t.created_at,
                        worktree: WorktreeState::None,
                        files: 0,
                        added: 0,
                        removed: 0,
                        agent_ms: 0,
                        counting: false,
                    },
                    |s| (*s).clone(),
                );
                let last = latest
                    .get(&t.id)
                    .map(|r| r.status.clone())
                    .or_else(|| summary.runs.last().map(|r| r.status.clone()));
                let status = card_status(
                    pending.contains(&t.id),
                    working.contains(&t.id),
                    queued.contains(&t.id),
                    last.as_ref(),
                );
                Card {
                    thread: t.id,
                    title: t.title.clone(),
                    created_at: t.created_at,
                    archived: t.archived_at.is_some(),
                    status,
                    last_activity: summary.last_activity_at.max(t.created_at),
                    branch: t.worktree.as_ref().map(|wt| wt.branch.clone()),
                    summary,
                }
            })
            .collect();
        let view = &mut self.channel_view;
        let rows = arrange(
            &cards,
            &Shown {
                filter: view.filter,
                sort: view.sort,
                query: &key.query,
                folded: Some(&view.folded),
            },
            now,
        );
        view.built = Some(key);
        view.counts = filter_counts(&cards);
        view.scale = cards.iter().map(|c| c.summary.agent_ms).max().unwrap_or(0);
        view.cards = cards.into_iter().map(|c| (c.thread, c)).collect();
        let to_top = std::mem::take(&mut view.to_top);
        if rows != view.rows {
            let top = view.list.logical_scroll_top();
            view.list.reset(rows.len());
            if !to_top {
                view.list.scroll_to(ListOffset {
                    item_ix: top.item_ix.min(rows.len().saturating_sub(1)),
                    offset_in_item: top.offset_in_item,
                });
            }
            view.rows = rows;
        } else if to_top {
            view.list.scroll_to(ListOffset::default());
        }
    }

    pub(super) fn set_channel_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.channel_view.filter = filter;
        self.channel_view.to_top = true;
        cx.notify();
    }

    pub(super) fn set_channel_sort(&mut self, sort: Sort, cx: &mut Context<Self>) {
        self.channel_view.sort = sort;
        self.channel_view.to_top = true;
        cx.notify();
    }

    fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let folded = &mut self.channel_view.folded;
        if !folded.remove(&section) {
            folded.insert(section);
        }
        cx.notify();
    }

    /// The channel view: header, stats, toolbar and the card list. An
    /// empty channel shows how to start instead, and a new thread is
    /// written above the cards.
    pub(super) fn render_channel_view(
        &self,
        channel: ChannelId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_kit::prelude::FluentBuilder as _;
        let mode = self.composer_mode();
        let empty = mode == ComposerMode::Empty;
        let drafting = mode == ComposerMode::NewThread;
        let plain = !empty && !drafting;
        div()
            .id("channel-view")
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_channel_header(channel, empty, cx))
            .when(plain, |view| {
                view.child(self.render_channel_stats())
                    .child(self.render_channel_toolbar(cx))
            })
            .children(
                self.channel_view
                    .error
                    .as_ref()
                    .filter(|_| !empty)
                    .map(|error| {
                        let text = format!("Couldn't load this channel. {error}");
                        div()
                            .id("channel-error")
                            .flex_none()
                            .px(px(24.))
                            .pb(px(8.))
                            .text_size(px(12.))
                            .text_color(color("danger"))
                            .aria_label(text.clone())
                            .child(text)
                            .test_support()
                    }),
            )
            .when(drafting, |view| {
                view.child(self.render_new_thread_composer())
            })
            .child(if empty {
                self.render_channel_empty(cx).into_any_element()
            } else {
                div()
                    .id("channel-cards")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    // Written above, the cards step back.
                    .when(drafting, |cards| cards.pt(px(20.)).opacity(0.45))
                    .child(
                        list(
                            self.channel_view.list.clone(),
                            cx.processor(|this, ix, _, cx| this.render_channel_row(ix, cx)),
                        )
                        .size_full(),
                    )
                    .test_support()
                    .into_any_element()
            })
            .test_support()
            .into_any_element()
    }

    fn render_channel_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.channel_view.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        let body = match row {
            Row::Section {
                section,
                count,
                open,
                first,
            } => self
                .render_section(section, count, open, first, cx)
                .into_any_element(),
            Row::Cards(left, right) => div()
                .flex()
                .gap(px(12.))
                .pb(px(12.))
                .child(
                    div()
                        .flex_1()
                        .flex_basis(px(0.))
                        .min_w_0()
                        .child(self.render_card(left, cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .flex_basis(px(0.))
                        .min_w_0()
                        .children(right.map(|t| self.render_card(t, cx))),
                )
                .into_any_element(),
            Row::Nothing => {
                let searching = !self.channel_view.search.read(cx).value().trim().is_empty();
                let text = if searching {
                    "No threads match."
                } else {
                    "Nothing here."
                };
                div()
                    .id("channel-nothing")
                    .py(px(16.))
                    .text_color(color("text-tertiary"))
                    .aria_label(text)
                    .child(text)
                    .test_support()
                    .into_any_element()
            }
        };
        div().px(px(24.)).child(body).into_any_element()
    }

    fn render_section(
        &self,
        section: Section,
        count: usize,
        open: bool,
        first: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use gpui_kit::component::{Icon, IconName};
        use gpui_kit::prelude::FluentBuilder as _;
        let older = section == Section::Older;
        let mut label = format!("{} {count}", section.title());
        if older {
            label.push_str(" · last activity over 7 days ago");
        }
        let dot = match section {
            Section::NeedsYou => Some("warning"),
            Section::Working => Some("accent-blue"),
            _ => None,
        };
        div()
            .id(SharedString::from(format!(
                "channel-section-{}",
                section.key()
            )))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .px(px(8.))
            .when(!first, |row| row.mt(px(8.)))
            .mb(px(4.))
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|row| {
                row.bg(color("surface-100"))
                    .text_color(color("text-primary"))
            })
            .text_size(px(11.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(color("text-tertiary"))
            .role(Role::Button)
            .aria_label(label)
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(14.)),
            )
            .children(dot.map(super::common::dot))
            .child(section.title().to_uppercase())
            .child(
                div()
                    .text_color(color("text-secondary"))
                    .child(count.to_string()),
            )
            .when(older, |row| {
                row.child(
                    div()
                        .ml(px(4.))
                        .font_weight(FontWeight::NORMAL)
                        .child("· last activity over 7 days ago"),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(section, cx)))
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crew_core::event::RunStatus;
    use crew_core::ids::ThreadId;
    use crew_proto::{RunBar, ThreadSummary, WorktreeState};

    use super::{
        Card, CardStatus, Filter, Row, Section, Shown, Sort, WEEK_MS, arrange, card_status,
        card_time, counts_line, filter_counts, retry_after, stop_label, strip_layout, thousands,
        working_line, worktree_word,
    };
    use crate::ui::run_card::{Step, StepState};

    const NOW: i64 = 1_800_000_000_000;
    const MIN: i64 = 60_000;

    fn summary(thread: ThreadId) -> ThreadSummary {
        ThreadSummary {
            thread,
            messages: 0,
            runs: vec![],
            handoffs: 0,
            participants: vec![],
            excerpt: String::new(),
            last_activity_at: 0,
            worktree: WorktreeState::None,
            files: 0,
            added: 0,
            removed: 0,
            agent_ms: 0,
            counting: false,
        }
    }

    fn card(title: &str, status: CardStatus, created_at: i64, last_activity: i64) -> Card {
        let thread = ThreadId::new();
        Card {
            thread,
            title: title.into(),
            created_at,
            archived: false,
            status,
            last_activity,
            summary: summary(thread),
            branch: None,
        }
    }

    fn titles(rows: &[Row], cards: &[Card]) -> Vec<String> {
        let name = |t: ThreadId| cards.iter().find(|c| c.thread == t).unwrap().title.clone();
        rows.iter()
            .flat_map(|r| match r {
                Row::Section {
                    section,
                    count,
                    open,
                    ..
                } => vec![format!(
                    "[{} {count}{}]",
                    section.title(),
                    if *open { "" } else { " folded" }
                )],
                Row::Cards(a, b) => std::iter::once(name(*a)).chain(b.map(name)).collect(),
                Row::Nothing => vec!["nothing".into()],
            })
            .collect()
    }

    #[test]
    fn a_status_puts_asks_first_then_live_runs_then_the_last_end() {
        assert_eq!(card_status(true, true, false, None), CardStatus::NeedsYou);
        assert_eq!(
            card_status(false, true, true, Some(&RunStatus::Done)),
            CardStatus::Working
        );
        assert_eq!(
            card_status(false, false, true, Some(&RunStatus::Done)),
            CardStatus::Queued,
            "waiting for a slot isn't working"
        );
        assert_eq!(
            card_status(false, false, false, Some(&RunStatus::Done)),
            CardStatus::Done
        );
        assert_eq!(
            card_status(false, false, false, Some(&RunStatus::Interrupted)),
            CardStatus::Stopped
        );
        assert_eq!(
            card_status(false, false, false, Some(&RunStatus::Failed)),
            CardStatus::Failed
        );
        assert_eq!(card_status(false, false, false, None), CardStatus::Idle);
    }

    #[test]
    fn stop_names_one_agent_or_says_all() {
        assert_eq!(stop_label(&[]), None);
        assert_eq!(stop_label(&["Forge".into()]).as_deref(), Some("Stop Forge"));
        assert_eq!(
            stop_label(&["Forge".into(), "Scout".into()]).as_deref(),
            Some("Stop all in thread")
        );
    }

    #[test]
    fn failed_overviews_back_off() {
        let secs: Vec<u64> = (1..=6).map(|n| retry_after(n).as_secs()).collect();
        assert_eq!(secs, [2, 5, 15, 30, 30, 30]);
    }

    #[test]
    fn all_groups_cards_in_sections_with_older_folded() {
        let cards = vec![
            card("old", CardStatus::Done, 0, NOW - 8 * 24 * 60 * MIN),
            card("done", CardStatus::Done, 0, NOW - MIN),
            card("ask", CardStatus::NeedsYou, 0, NOW - 3 * WEEK_MS),
            card("busy", CardStatus::Working, 0, NOW - 2 * MIN),
            card("stopped", CardStatus::Stopped, 0, NOW - 5 * MIN),
        ];
        let folded = HashSet::from([Section::Older]);
        let shown = Shown {
            folded: Some(&folded),
            ..Shown::default()
        };
        let rows = arrange(&cards, &shown, NOW);
        assert_eq!(
            titles(&rows, &cards),
            [
                "[Needs you 1]",
                "ask",
                "[Working 1]",
                "busy",
                "[Recent 2]",
                "done",
                "stopped",
                "[Older 1 folded]"
            ]
        );
        assert!(matches!(rows[0], Row::Section { first: true, .. }));
        assert!(matches!(rows[2], Row::Section { first: false, .. }));
        let open = arrange(&cards, &Shown::default(), NOW);
        assert_eq!(titles(&open, &cards).last().unwrap(), "old");
    }

    #[test]
    fn other_filters_list_flat_and_archived_only_under_archived() {
        let mut cards = vec![
            card("a", CardStatus::Working, 0, NOW),
            card("b", CardStatus::Done, 0, NOW - MIN),
            card("c", CardStatus::Working, 0, NOW - 2 * MIN),
            card("gone", CardStatus::Working, 0, NOW - 3 * MIN),
        ];
        cards[3].archived = true;
        let working = Shown {
            filter: Filter::Working,
            ..Shown::default()
        };
        assert_eq!(titles(&arrange(&cards, &working, NOW), &cards), ["a", "c"]);
        let archived = Shown {
            filter: Filter::Archived,
            ..Shown::default()
        };
        assert_eq!(titles(&arrange(&cards, &archived, NOW), &cards), ["gone"]);
        let done = Shown {
            filter: Filter::Done,
            query: "zzz",
            ..Shown::default()
        };
        assert_eq!(titles(&arrange(&cards, &done, NOW), &cards), ["nothing"]);
        assert_eq!(filter_counts(&cards), [3, 2, 0, 1, 1]);
    }

    #[test]
    fn search_reads_titles_and_excerpts_in_any_case() {
        let mut cards = vec![
            card("Fix the LOGIN", CardStatus::Idle, 0, NOW),
            card("docs", CardStatus::Idle, 0, NOW - MIN),
            card("other", CardStatus::Idle, 0, NOW),
        ];
        cards[1].summary.excerpt = "The login page needs words".into();
        let shown = Shown {
            filter: Filter::Archived,
            query: " login ",
            ..Shown::default()
        };
        assert_eq!(titles(&arrange(&cards, &shown, NOW), &cards), ["nothing"]);
        let shown = Shown {
            query: " login ",
            ..Shown::default()
        };
        let rows = arrange(&cards, &shown, NOW);
        assert_eq!(
            titles(&rows, &cards),
            ["[Recent 2]", "Fix the LOGIN", "docs"]
        );
    }

    #[test]
    fn sorts_by_activity_creation_or_changes() {
        let mut cards = vec![
            card("first", CardStatus::Done, 1, NOW - MIN),
            card("second", CardStatus::Done, 2, NOW - 3 * MIN),
            card("third", CardStatus::Done, 3, NOW - 2 * MIN),
        ];
        cards[1].summary.added = 100;
        cards[2].summary.removed = 10;
        let order = |sort| {
            let shown = Shown {
                filter: Filter::Done,
                sort,
                ..Shown::default()
            };
            titles(&arrange(&cards, &shown, NOW), &cards)
        };
        assert_eq!(order(Sort::Latest), ["first", "third", "second"]);
        assert_eq!(order(Sort::Created), ["third", "second", "first"]);
        assert_eq!(order(Sort::Changes), ["second", "third", "first"]);
    }

    #[test]
    fn counts_read_in_singular_and_plural() {
        let mut s = summary(ThreadId::new());
        assert_eq!(
            counts_line(&s, None),
            "0 messages · 0 runs · 0 handoffs · 0 files · 0s agent time"
        );
        s.messages = 1;
        s.runs = vec![RunBar {
            status: RunStatus::Done,
            ms: 22 * MIN,
        }];
        s.handoffs = 1;
        s.files = 1;
        s.added = 1204;
        s.removed = 3;
        s.agent_ms = 22 * MIN;
        assert_eq!(
            counts_line(&s, None),
            "1 message · 1 run · 1 handoff · 1 file +1,204 −3 · 22m agent time"
        );
        assert_eq!(
            counts_line(&s, Some("3m 12s")),
            "1 message · 1 run · 1 handoff · 1 file +1,204 −3 · running 3m 12s"
        );
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn the_worktree_reads_as_uncommitted_committed_or_clean() {
        assert_eq!(
            worktree_word(WorktreeState::Uncommitted { files: 4 }, true).0,
            "4 uncommitted"
        );
        assert_eq!(worktree_word(WorktreeState::Committed, true).0, "committed");
        assert_eq!(worktree_word(WorktreeState::Clean, true).0, "clean");
        assert_eq!(worktree_word(WorktreeState::None, true).0, "clean");
        assert_eq!(
            worktree_word(WorktreeState::None, false).0,
            "No worktree changes"
        );
    }

    #[test]
    fn card_times_read_relative_then_as_dates() {
        use chrono::{Local, TimeZone as _};
        assert_eq!(card_time(NOW - 20_000, NOW), "just now");
        assert_eq!(card_time(NOW - 2 * MIN, NOW), "2m ago");
        assert_eq!(card_time(NOW - 90 * MIN, NOW), "1h ago");
        let noon = Local
            .with_ymd_and_hms(2026, 11, 24, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let yesterday_morning = Local
            .with_ymd_and_hms(2026, 11, 23, 8, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        assert_eq!(card_time(yesterday_morning, noon), "Yesterday");
        let earlier = Local
            .with_ymd_and_hms(2026, 11, 3, 8, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        assert_eq!(card_time(earlier, noon), "Nov 3");
        let last_year = Local
            .with_ymd_and_hms(2025, 12, 30, 8, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        assert_eq!(card_time(last_year, noon), "Dec 30, 2025");
    }

    #[test]
    fn a_working_card_says_what_the_agent_is_doing() {
        let step = |state, verb: &str| Step {
            state,
            verb: verb.into(),
            target: "cargo test".into(),
            lines: None,
            reason: None,
            raw: None,
            answer: None,
            outside: false,
        };
        assert_eq!(
            working_line("Forge", Some(&step(StepState::Running, "Running"))),
            "Forge is running cargo test"
        );
        assert_eq!(
            working_line("Forge", Some(&step(StepState::Done, "Ran"))),
            "Forge is working."
        );
        assert_eq!(working_line("Forge", None), "Forge is working.");
    }

    #[test]
    fn run_strips_scale_each_run_with_a_floor() {
        let widths = |l: Vec<(usize, f32)>| -> Vec<f32> { l.into_iter().map(|(_, w)| w).collect() };
        assert_eq!(
            widths(strip_layout(&[50, 25, 0], 1000, 112., 2.)),
            [5.6, 3., 3.]
        );
        assert_eq!(widths(strip_layout(&[10], 0, 112., 2.)), [112.]);
    }

    #[test]
    fn a_full_strip_shrinks_then_drops_the_oldest_runs() {
        // 20 runs of 5 px with 2 px gaps need 138 px: they shrink alike.
        let layout = strip_layout(&[5; 20], 112, 112., 2.);
        assert_eq!(layout.len(), 20);
        let total: f32 = layout.iter().map(|(_, w)| w).sum::<f32>() + 2. * 19.;
        assert!(total <= 112.01, "{total}");
        assert!(layout.iter().all(|(_, w)| *w >= 2.));
        // 60 runs can't fit even at 2 px: the oldest go, the newest stay.
        let layout = strip_layout(&[5; 60], 112, 112., 2.);
        assert_eq!(layout.last().unwrap().0, 59);
        assert_eq!(layout.len(), 28, "28 × 2 px + 27 × 2 px gaps = 110 px");
        assert_eq!(layout[0].0, 32);
    }
}
