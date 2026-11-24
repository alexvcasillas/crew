//! What crewd says about each thread (changes, runs, activity), kept for
//! the threads sidebar. Everything is asked for once, then only the
//! channels crewd reported changes in, after changes pause.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crew_core::ids::{ChannelId, ThreadId};
use crew_proto::{Request, ThreadSummary};
use gpui_kit::*;

use super::super::channel_view::retry_after;
use super::super::workspace::Workspace;
use super::threads::sidebar_mode;
use crate::prefs::SidebarMode;

/// How long changes must pause before their channels are asked for.
pub(crate) const SETTLE: Duration = Duration::from_millis(500);
/// The longest changes wait while they keep coming.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(2);
/// Asks again if crewd never says counts it was working out are done.
const COUNTING_FALLBACK: Duration = Duration::from_secs(3);

/// The summaries and how they are kept up to date.
pub(crate) struct Summaries {
    pub(super) by_thread: HashMap<ThreadId, ThreadSummary>,
    /// Moves whenever `by_thread` changes.
    pub(super) generation: u64,
    fetch: Option<Task<()>>,
    /// Waits for changes to pause.
    refetch: Option<Task<()>>,
    fallback: Option<Task<()>>,
    retry: Option<Task<()>>,
    failures: u32,
    /// Every channel is asked for next: at first, after a reconnect or a
    /// resync.
    full: bool,
    /// Channels with changes not asked for yet.
    dirty: HashSet<ChannelId>,
    /// When the oldest of those changes came.
    dirty_since: Option<Instant>,
    /// Ask at once, as when the list shows again.
    urgent: bool,
}

impl Default for Summaries {
    fn default() -> Self {
        Self {
            by_thread: HashMap::new(),
            generation: 0,
            fetch: None,
            refetch: None,
            fallback: None,
            retry: None,
            failures: 0,
            full: true,
            dirty: HashSet::new(),
            dirty_since: None,
            urgent: false,
        }
    }
}

impl Summaries {
    /// The list shows again: changes made meanwhile are asked for at
    /// once.
    pub(super) fn shown_again(&mut self) {
        self.urgent = true;
    }

    fn wanted(&self) -> bool {
        self.full || !self.dirty.is_empty()
    }
}

/// How long to wait before asking, the first change having come at
/// `since` and the latest at `now`: [`SETTLE`] after the latest, but no
/// later than [`MAX_WAIT`] after the first.
pub(crate) fn settle_wait(since: Instant, now: Instant) -> Duration {
    let left = MAX_WAIT.saturating_sub(now.saturating_duration_since(since));
    SETTLE.min(left)
}

impl Workspace {
    /// Notes what crewd reported: the channels of `threads` and the
    /// `stale` ones are asked for again once changes pause; `full` (a
    /// snapshot) asks for everything.
    pub(crate) fn threads_touched(
        &mut self,
        threads: &[ThreadId],
        stale: &[ChannelId],
        full: bool,
        cx: &mut Context<Self>,
    ) {
        let channels: Vec<ChannelId> = threads
            .iter()
            .filter_map(|t| self.state().thread(*t).map(|t| t.channel))
            .chain(stale.iter().copied())
            .collect();
        let s = &mut self.sidebar.threads.summaries;
        if full {
            s.full = true;
            s.dirty.clear();
        } else if !channels.is_empty() {
            s.dirty.extend(channels);
        } else {
            return;
        }
        let now = cx.background_executor().now();
        s.dirty_since.get_or_insert(now);
        if sidebar_mode(cx) == SidebarMode::Threads && s.fetch.is_none() && s.retry.is_none() {
            self.schedule_summaries(cx);
        }
    }

    /// Asks once changes pause for [`SETTLE`], each change pushing the
    /// wait back, but no more than [`MAX_WAIT`] after the first.
    fn schedule_summaries(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let s = &mut self.sidebar.threads.summaries;
        let wait = settle_wait(*s.dirty_since.get_or_insert(now), now);
        s.refetch = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |this, cx| {
                let s = &mut this.sidebar.threads.summaries;
                s.refetch = None;
                if s.fetch.is_none() && s.wanted() {
                    this.fetch_thread_summaries(cx);
                }
            })
            .ok();
        }));
    }

    /// Asks for what is wanted when the threads sidebar shows: everything
    /// the first time, at once what changed while it was hidden.
    pub(crate) fn sync_thread_summaries(&mut self, cx: &mut Context<Self>) {
        if sidebar_mode(cx) != SidebarMode::Threads {
            return;
        }
        let s = &mut self.sidebar.threads.summaries;
        if s.fetch.is_some() || s.retry.is_some() || !s.wanted() {
            return;
        }
        let first = s.full && s.generation == 0;
        if first || std::mem::take(&mut s.urgent) {
            self.fetch_thread_summaries(cx);
        } else if s.refetch.is_none() {
            self.schedule_summaries(cx);
        }
    }

    /// Asks crewd for everything, or for the changed channels only, and
    /// merges the answer in.
    fn fetch_thread_summaries(&mut self, cx: &mut Context<Self>) {
        let s = &mut self.sidebar.threads.summaries;
        let channels: Option<Vec<ChannelId>> =
            (!s.full).then(|| s.dirty.drain().collect::<Vec<_>>());
        s.full = false;
        s.dirty.clear();
        s.dirty_since = None;
        s.refetch = None;
        s.urgent = false;
        let answer = self.client.request(Request::ThreadSummaries {
            project: None,
            channels: channels.clone(),
        });
        self.sidebar.threads.summaries.fetch = Some(cx.spawn(async move |this, cx| {
            let got = answer.await.and_then(|v| {
                serde_json::from_value::<Vec<ThreadSummary>>(v).map_err(|e| e.to_string())
            });
            this.update(cx, |this, cx| {
                this.sidebar.threads.summaries.fetch = None;
                match got {
                    Ok(summaries) => this.take_summaries(channels, summaries, cx),
                    Err(e) => {
                        eprintln!("crew: couldn't load the threads: {e}");
                        let s = &mut this.sidebar.threads.summaries;
                        match channels {
                            Some(asked) => s.dirty.extend(asked),
                            None => s.full = true,
                        }
                        s.failures += 1;
                        let wait = retry_after(s.failures);
                        s.retry = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(wait).await;
                            this.update(cx, |this, cx| {
                                let s = &mut this.sidebar.threads.summaries;
                                s.retry = None;
                                s.urgent = true;
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

    /// Takes in crewd's answer for `channels` (all when `None`).
    fn take_summaries(
        &mut self,
        channels: Option<Vec<ChannelId>>,
        summaries: Vec<ThreadSummary>,
        cx: &mut Context<Self>,
    ) {
        let counting: HashSet<ChannelId> = summaries
            .iter()
            .filter(|s| s.counting)
            .filter_map(|s| self.state().thread(s.thread).map(|t| t.channel))
            .collect();
        let channel_of: HashMap<ThreadId, ChannelId> = match &channels {
            Some(_) => self
                .sidebar
                .threads
                .summaries
                .by_thread
                .keys()
                .filter_map(|t| self.state().thread(*t).map(|th| (*t, th.channel)))
                .collect(),
            None => HashMap::new(),
        };
        let s = &mut self.sidebar.threads.summaries;
        s.failures = 0;
        match channels {
            None => s.by_thread.clear(),
            Some(asked) => s
                .by_thread
                .retain(|t, _| channel_of.get(t).is_some_and(|c| !asked.contains(c))),
        }
        s.by_thread
            .extend(summaries.into_iter().map(|x| (x.thread, x)));
        s.generation += 1;
        if !counting.is_empty() {
            s.fallback = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(COUNTING_FALLBACK).await;
                this.update(cx, |this, cx| {
                    let s = &mut this.sidebar.threads.summaries;
                    s.fallback = None;
                    s.dirty.extend(counting);
                    this.sync_thread_summaries(cx);
                })
                .ok();
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{MAX_WAIT, SETTLE, settle_wait};

    #[test]
    fn changes_settle_but_never_wait_past_the_cap() {
        let start = Instant::now();
        assert_eq!(settle_wait(start, start), SETTLE);
        let late = start + MAX_WAIT - Duration::from_millis(200);
        assert_eq!(settle_wait(start, late), Duration::from_millis(200));
        assert_eq!(settle_wait(start, start + MAX_WAIT * 2), Duration::ZERO);
    }
}
