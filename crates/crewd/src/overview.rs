//! A channel at a glance: stats and one summary per thread.
//!
//! Change counts come from git, so they are cached per thread and only
//! worked out again once something may have changed them: a run ended or
//! edited a file, a commit, a discard or a worktree setup. An overview
//! waits a short while for counts it lacks; the rest keep being worked out
//! in the background and the overview says so.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crew_core::event::RunStatus;
use crew_core::ids::{ChannelId, ProjectId, ThreadId};
use crew_core::project_kind::{ProjectKind, WorkMode};
use crew_core::store::{StoredRun, StoredThread, ThreadActivity, ThreadWorktree};
use crew_core::text::clean_line;
use crew_core::worktree::ChangeCounts;
use crew_proto::{ChannelOverview, ChannelStats, RunBar, ThreadSummary, WorktreeState};

use crate::service::{Reply, Service, json, log, now_ms};

/// Longest excerpt, in characters.
pub const EXCERPT_LEN: usize = 240;
/// What "this week" covers.
pub(crate) const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Threads whose counts are worked out at once, across all overviews.
pub(crate) const GIT_AT_ONCE: usize = 8;
/// How long an overview waits for counts it lacks.
const OVERVIEW_WAIT: Duration = Duration::from_millis(1500);
/// How often a live run's edits may drop its thread's counts.
const LIVE_EVERY: Duration = Duration::from_secs(5);
/// How often counts are worked out again when the thread keeps changing
/// meanwhile.
const TRIES: u32 = 3;

/// Change counts per thread. Each thread has a generation that grows when
/// its counts may have changed, so counts worked out before that are
/// never saved as current. The last counts stay, to show while new ones
/// are worked out.
#[derive(Default)]
pub(crate) struct ChangeCache {
    generation: HashMap<ThreadId, u64>,
    counts: HashMap<ThreadId, (u64, ChangeCounts)>,
    /// Threads whose counts are being worked out.
    computing: HashSet<ThreadId>,
    /// When a live run last dropped each thread's counts.
    live: HashMap<ThreadId, Instant>,
    /// Threads with a live edit that came too soon after the last drop.
    live_pending: HashSet<ThreadId>,
    /// Edit and write calls of live runs that haven't finished, by thread.
    edits: HashSet<(ThreadId, String)>,
    /// Channels with a waiter that tells clients once the counts an
    /// overview left out are worked out.
    waiting: HashSet<ChannelId>,
}

/// What a live run's edit did to its thread's counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveEdit {
    /// They are no longer current.
    Dropped,
    /// Too soon after the last drop: they drop after `wait`. `first` is
    /// set for the first edit held back since then.
    Held { wait: Duration, first: bool },
}

impl ChangeCache {
    fn generation(&self, thread: ThreadId) -> u64 {
        self.generation.get(&thread).copied().unwrap_or(0)
    }

    /// The counts, if worked out since the last change.
    pub(crate) fn fresh(&self, thread: ThreadId) -> Option<ChangeCounts> {
        let (generation, counts) = self.counts.get(&thread)?;
        (*generation == self.generation(thread)).then_some(*counts)
    }

    /// The last counts worked out, current or not.
    fn last(&self, thread: ThreadId) -> Option<ChangeCounts> {
        self.counts.get(&thread).map(|(_, c)| *c)
    }

    /// Marks the thread's counts as being worked out and returns the
    /// generation they are for, unless that is already under way.
    pub(crate) fn claim(&mut self, thread: ThreadId) -> Option<u64> {
        self.computing
            .insert(thread)
            .then(|| self.generation(thread))
    }

    /// Saves counts worked out at `generation`. If the thread changed
    /// since, nothing is saved and the new generation is returned, still
    /// claimed, to work them out again; with `retry` off it is released.
    pub(crate) fn finish(
        &mut self,
        thread: ThreadId,
        generation: u64,
        counts: ChangeCounts,
        retry: bool,
    ) -> Option<u64> {
        let now = self.generation(thread);
        if generation == now {
            self.counts.insert(thread, (generation, counts));
        } else if retry {
            return Some(now);
        }
        self.computing.remove(&thread);
        None
    }

    /// Drops the thread's counts as current.
    pub(crate) fn invalidate(&mut self, thread: ThreadId) {
        *self.generation.entry(thread).or_default() += 1;
        self.live_pending.remove(&thread);
    }

    /// Drops the thread's counts for a live run's edit, at most once per
    /// [`LIVE_EVERY`]. An edit that comes sooner is kept for
    /// [`Self::apply_due`]. Returns whether it dropped them.
    pub(crate) fn invalidate_live(&mut self, thread: ThreadId, now: Instant) -> LiveEdit {
        if let Some(at) = self.live.get(&thread) {
            let since = now.saturating_duration_since(*at);
            if since < LIVE_EVERY {
                let first = self.live_pending.insert(thread);
                return LiveEdit::Held {
                    wait: LIVE_EVERY - since,
                    first,
                };
            }
        }
        self.live.insert(thread, now);
        self.invalidate(thread);
        LiveEdit::Dropped
    }

    /// Drops the thread's counts for a held back live edit once
    /// [`LIVE_EVERY`] has passed. Returns whether it did.
    pub(crate) fn apply_due(&mut self, thread: ThreadId, now: Instant) -> bool {
        self.live_pending.contains(&thread)
            && self.invalidate_live(thread, now) == LiveEdit::Dropped
    }

    /// Notes an edit or write call of a live run.
    pub(crate) fn edit_started(&mut self, thread: ThreadId, call: String) {
        self.edits.insert((thread, call));
    }

    /// Forgets the thread's unfinished edit calls, as when its run ended.
    pub(crate) fn forget_edits(&mut self, thread: ThreadId) {
        self.edits.retain(|(t, _)| *t != thread);
    }

    /// Whether `call` was an edit or write call. Forgets it.
    pub(crate) fn edit_finished(&mut self, thread: ThreadId, call: &str) -> bool {
        self.edits.remove(&(thread, call.to_string()))
    }
}

impl Service {
    /// Answers `ChannelOverview`. Git only runs for threads that aren't
    /// archived and whose counts are not current, a few at a time, and a
    /// git failure only zeroes that thread's counts. Counts not worked out
    /// within [`OVERVIEW_WAIT`] come as their last value, or zeros, marked
    /// `counting`.
    pub(crate) async fn channel_overview(&self, channel: ChannelId) -> Reply {
        json(self.channel_overview_data(channel).await?)
    }

    /// The overview [`Self::channel_overview`] answers with.
    pub(crate) async fn channel_overview_data(
        &self,
        channel: ChannelId,
    ) -> Result<ChannelOverview, String> {
        let (threads, activity, runs, project) = {
            let db = self.db();
            let found = db.store.channel(channel).and_then(|c| {
                Ok((
                    db.store.threads(channel)?,
                    db.store.thread_activity(channel)?,
                    db.store.channel_runs(channel)?,
                    db.store.project(c.project)?,
                ))
            });
            found.map_err(|e| e.to_string())?
        };
        let copied = project.copy_files.clone();
        // What each thread's changes are counted on: its worktree, or in
        // place the files its runs touched. Read before the cache is locked.
        let mut targets: HashMap<ThreadId, CountTarget> = HashMap::new();
        for thread in &threads {
            match &thread.worktree {
                Some(wt) => {
                    targets.insert(thread.id, CountTarget::Copy(wt.clone()));
                }
                None if project.work_mode == WorkMode::InPlace && thread.archived_at.is_none() => {
                    let edits = self.thread_edits(thread.id, &project.root)?;
                    if !edits.is_empty() {
                        targets.insert(
                            thread.id,
                            CountTarget::InPlace {
                                root: project.root.clone(),
                                kind: project.kind.clone(),
                                edits,
                            },
                        );
                    }
                }
                None => {}
            }
        }
        let deadline = tokio::time::Instant::now() + OVERVIEW_WAIT;

        let mut wanted = Vec::new();
        {
            let mut cache = self.changes();
            let now = Instant::now();
            for thread in &threads {
                let Some(target) = targets.get(&thread.id) else {
                    continue;
                };
                cache.apply_due(thread.id, now);
                if thread.archived_at.is_some() || cache.fresh(thread.id).is_some() {
                    continue;
                }
                wanted.push(thread.id);
                if let Some(generation) = cache.claim(thread.id) {
                    self.count_in_background(thread.id, generation, target.clone(), copied.clone());
                }
            }
        }

        loop {
            let counted = self.counted();
            tokio::pin!(counted);
            counted.as_mut().enable();
            let all_counted = {
                let cache = self.changes();
                wanted.iter().all(|t| cache.fresh(*t).is_some())
            };
            if all_counted {
                break;
            }
            if tokio::time::timeout_at(deadline, counted).await.is_err() {
                break;
            }
        }

        let late: Vec<ThreadId> = {
            let cache = self.changes();
            wanted
                .into_iter()
                .filter(|t| cache.fresh(*t).is_none())
                .collect()
        };
        if !late.is_empty() {
            self.tell_when_counted(channel, late);
        }

        let mut counts = HashMap::new();
        let mut counting = HashSet::new();
        {
            let cache = self.changes();
            for thread in &threads {
                let Some(target) = targets.get(&thread.id) else {
                    continue;
                };
                let c = match cache.fresh(thread.id) {
                    Some(c) => c,
                    None => {
                        if thread.archived_at.is_none() {
                            counting.insert(thread.id);
                        }
                        cache.last(thread.id).unwrap_or_default()
                    }
                };
                // A folder deleted outside Crew since the counts were made.
                let c = if target.exists() {
                    c
                } else {
                    counting.remove(&thread.id);
                    ChangeCounts::default()
                };
                counts.insert(thread.id, c);
            }
        }

        Ok(summarise(
            &threads,
            &activity,
            &runs,
            &counts,
            &counting,
            now_ms(),
        ))
    }

    /// Answers `ThreadSummaries`: the summaries of every thread that
    /// isn't archived, in `project` or in every project, from all their
    /// channels' overviews at once.
    pub(crate) async fn thread_summaries(&self, project: Option<ProjectId>) -> Reply {
        let (channels, archived) = self
            .with_reader(|store| {
                // Removed projects are left out.
                let projects = store.projects()?;
                let mut channels = Vec::new();
                let mut archived = HashSet::new();
                for p in projects
                    .iter()
                    .filter(|p| project.is_none_or(|id| id == p.id))
                {
                    for c in store.channels(p.id)? {
                        archived.extend(
                            store
                                .threads(c.id)?
                                .into_iter()
                                .filter(|t| t.archived_at.is_some())
                                .map(|t| t.id),
                        );
                        channels.push(c.id);
                    }
                }
                Ok::<_, crew_core::store::StoreError>((channels, archived))
            })
            .map_err(|e| e.to_string())?;
        let mut jobs = tokio::task::JoinSet::new();
        for (i, channel) in channels.into_iter().enumerate() {
            let svc = self.clone();
            jobs.spawn(async move { (i, svc.channel_overview_data(channel).await) });
        }
        let mut overviews = Vec::new();
        while let Some(done) = jobs.join_next().await {
            let (i, overview) = done.map_err(|e| format!("The overview task ended: {e}"))?;
            overviews.push((i, overview?));
        }
        overviews.sort_by_key(|(i, _)| *i);
        let summaries: Vec<ThreadSummary> = overviews
            .into_iter()
            .flat_map(|(_, o)| o.threads)
            .filter(|t| !archived.contains(&t.thread))
            .collect();
        json(summaries)
    }

    /// Sends `ChannelOverviewStale` once none of `threads` is being
    /// counted any more. One waiter per channel: a second overview that
    /// leaves counts out while one waits adds nothing.
    fn tell_when_counted(&self, channel: ChannelId, threads: Vec<ThreadId>) {
        if !self.changes().waiting.insert(channel) {
            return;
        }
        let svc = self.clone();
        tokio::spawn(async move {
            loop {
                let counted = svc.counted();
                tokio::pin!(counted);
                counted.as_mut().enable();
                let done = {
                    let mut cache = svc.changes();
                    let done = threads.iter().all(|t| !cache.computing.contains(t));
                    if done {
                        cache.waiting.remove(&channel);
                    }
                    done
                };
                if done {
                    break;
                }
                counted.await;
            }
            svc.overview_stale(channel);
        });
    }

    /// Works out the thread's counts off the async threads, waiting for
    /// one of [`GIT_AT_ONCE`] slots, and saves them. Goes on even if the
    /// overview that asked stopped waiting.
    fn count_in_background(
        &self,
        thread: ThreadId,
        mut generation: u64,
        target: CountTarget,
        copied: Vec<String>,
    ) {
        let svc = self.clone();
        tokio::spawn(async move {
            for attempt in 1..=TRIES {
                let counts = match svc.count_slots().acquire_owned().await {
                    Ok(slot) => {
                        let delay = svc.count_delay();
                        let (target, copied) = (target.clone(), copied.clone());
                        let job = tokio::task::spawn_blocking(move || {
                            let _slot = slot;
                            std::thread::sleep(delay);
                            count_changes(thread, &target, &copied)
                        });
                        job.await.unwrap_or_else(|e| {
                            log(format!(
                                "counting the changes of thread {thread} ended: {e}"
                            ));
                            ChangeCounts::default()
                        })
                    }
                    Err(_) => ChangeCounts::default(),
                };
                let again = svc
                    .changes()
                    .finish(thread, generation, counts, attempt < TRIES);
                svc.notify_counted();
                match again {
                    Some(next) => generation = next,
                    None => break,
                }
            }
        });
    }
}

/// The thread's change counts, every repo's added up. A missing folder or
/// a git failure gives zeros; a failure is logged.
fn count_changes(thread: ThreadId, target: &CountTarget, copied: &[String]) -> ChangeCounts {
    if !target.exists() {
        return ChangeCounts::default();
    }
    let counted = match target {
        CountTarget::Copy(wt) => crate::folders::thread_counts(wt, copied),
        CountTarget::InPlace { root, kind, edits } => {
            crate::folders::in_place_counts(root, kind, edits)
        }
    };
    counted.unwrap_or_else(|e| {
        log(format!(
            "couldn't count the changes of thread {thread}: {e}"
        ));
        ChangeCounts::default()
    })
}

/// What a thread's changes are counted on.
#[derive(Clone)]
enum CountTarget {
    Copy(ThreadWorktree),
    /// The project folder, where the thread's runs touched `edits`.
    InPlace {
        root: std::path::PathBuf,
        kind: ProjectKind,
        edits: Vec<String>,
    },
}

impl CountTarget {
    fn exists(&self) -> bool {
        match self {
            CountTarget::Copy(wt) => wt.path.is_dir(),
            CountTarget::InPlace { root, .. } => root.is_dir(),
        }
    }
}

/// How long a run ran, or has run so far. A queued run, or one that ended
/// without starting its CLI, counts 0.
pub(crate) fn run_ms(run: &StoredRun, now: i64) -> i64 {
    match (&run.status, run.ended_at) {
        (RunStatus::Queued, _) => 0,
        (_, Some(_)) if run.session_id.is_empty() => 0,
        (_, Some(ended)) => (ended - run.started_at).max(0),
        (_, None) => (now - run.started_at).max(0),
    }
}

/// Whether the run counts for the span from `since` (Unix ms): it is
/// working, or it started and ended since then.
pub(crate) fn ran_since(run: &StoredRun, since: i64) -> bool {
    run.status == RunStatus::Working
        || (run.ended_at.is_some_and(|e| e >= since) && !run.session_id.is_empty())
}

fn worktree_state(thread: &StoredThread, counts: Option<&ChangeCounts>) -> WorktreeState {
    if thread.worktree.is_none() {
        return WorktreeState::None;
    }
    let c = counts.copied().unwrap_or_default();
    if c.uncommitted > 0 {
        WorktreeState::Uncommitted {
            files: c.uncommitted,
        }
    } else if c.committed {
        WorktreeState::Committed
    } else {
        WorktreeState::Clean
    }
}

/// Builds the overview from what the store and git said. Stats leave
/// archived threads out.
pub(crate) fn summarise(
    threads: &[StoredThread],
    activity: &[ThreadActivity],
    runs: &[StoredRun],
    counts: &HashMap<ThreadId, ChangeCounts>,
    counting: &HashSet<ThreadId>,
    now: i64,
) -> ChannelOverview {
    let since = now - WEEK_MS;
    let mut stats = ChannelStats::default();
    let mut summaries = Vec::with_capacity(threads.len());
    let activity: HashMap<ThreadId, &ThreadActivity> =
        activity.iter().map(|a| (a.thread, a)).collect();
    let mut runs_of: HashMap<ThreadId, Vec<&StoredRun>> = HashMap::new();
    for r in runs {
        runs_of.entry(r.thread).or_default().push(r);
    }
    for thread in threads {
        let act = activity.get(&thread.id).copied();
        let own = runs_of.remove(&thread.id).unwrap_or_default();
        let bars: Vec<RunBar> = own
            .iter()
            .map(|r| RunBar {
                status: r.status.clone(),
                ms: run_ms(r, now),
            })
            .collect();
        let mut participants = Vec::new();
        for r in &own {
            if !participants.contains(&r.agent) {
                participants.push(r.agent);
            }
        }
        let last_activity_at = own
            .iter()
            .map(|r| r.ended_at.unwrap_or(r.started_at))
            .chain(act.and_then(|a| a.last_message_at))
            .fold(thread.created_at, i64::max);
        let excerpt = act.map_or_else(String::new, |a| {
            pick_excerpt(
                a.pending_ask.as_deref(),
                a.latest_agent.as_deref(),
                a.first_you.as_deref(),
            )
        });
        let c = counts.get(&thread.id).copied().unwrap_or_default();
        let summary = ThreadSummary {
            thread: thread.id,
            messages: act.map_or(0, |a| a.messages),
            agent_ms: bars.iter().map(|b| b.ms).sum(),
            runs: bars,
            handoffs: thread.handoffs_used,
            participants,
            excerpt,
            last_activity_at,
            worktree: worktree_state(thread, counts.get(&thread.id)),
            files: c.files,
            added: c.added,
            removed: c.removed,
            counting: counting.contains(&thread.id),
        };

        if thread.archived_at.is_none() {
            stats.threads += 1;
            if thread.created_at >= since {
                stats.threads_week += 1;
            }
            if own
                .iter()
                .any(|r| matches!(r.status, RunStatus::Queued | RunStatus::Working))
            {
                stats.active += 1;
            }
            if act.is_some_and(|a| a.needs_you) {
                stats.needs_you += 1;
            }
            for r in &own {
                if ran_since(r, since) {
                    stats.runs_week += 1;
                    stats.agent_ms_week += run_ms(r, now);
                }
            }
            if last_activity_at >= since {
                stats.files_week += summary.files;
                stats.added_week += summary.added;
                stats.removed_week += summary.removed;
            }
        }
        summaries.push(summary);
    }
    ChannelOverview {
        stats,
        threads: summaries,
    }
}

/// The pending ask, else the latest agent reply, else your first message,
/// as plain text.
pub fn pick_excerpt(pending: Option<&str>, agent: Option<&str>, you: Option<&str>) -> String {
    pending
        .or(agent)
        .or(you)
        .map(plain_excerpt)
        .unwrap_or_default()
}

/// Markdown as one line of plain text, at most [`EXCERPT_LEN`]
/// characters: code fences, heading, quote and list marks, emphasis,
/// inline code ticks and link targets go.
pub fn plain_excerpt(text: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let line = strip_line_marks(line);
        let line = strip_links(line);
        let line: String = line
            .chars()
            .filter(|c| !matches!(c, '*' | '`'))
            .collect::<String>()
            .replace("__", "");
        words.extend(clean_line(&line).split_whitespace().map(str::to_string));
    }
    let joined = words.join(" ");
    if joined.chars().count() <= EXCERPT_LEN {
        return joined;
    }
    let cut: String = joined.chars().take(EXCERPT_LEN - 1).collect();
    format!("{}…", cut.trim_end())
}

/// A line without its leading heading, quote or list marks.
fn strip_line_marks(mut line: &str) -> &str {
    loop {
        let before = line;
        line = line.trim_start_matches('#').trim_start();
        line = line.strip_prefix('>').unwrap_or(line).trim_start();
        for mark in ["- ", "* ", "+ "] {
            line = line.strip_prefix(mark).unwrap_or(line);
        }
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        if digits > 0
            && let Some(rest) = line[digits..]
                .strip_prefix(". ")
                .or_else(|| line[digits..].strip_prefix(") "))
        {
            line = rest;
        }
        if line == before {
            return line;
        }
    }
}

/// `[text](target)` becomes `text`, and `![alt](src)` becomes `alt`.
fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let close = rest[open..].find("](").map(|i| open + i);
        let end = close.and_then(|c| rest[c..].find(')').map(|i| c + i));
        let (Some(close), Some(end)) = (close, end) else {
            break;
        };
        let before = rest[..open].strip_suffix('!').unwrap_or(&rest[..open]);
        out.push_str(before);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crew_core::ids::ThreadId;
    use crew_core::worktree::ChangeCounts;

    use crate::overview::{ChangeCache, EXCERPT_LEN, LiveEdit, pick_excerpt, plain_excerpt};

    fn files(n: u32) -> ChangeCounts {
        ChangeCounts {
            files: n,
            ..ChangeCounts::default()
        }
    }

    #[test]
    fn counts_are_worked_out_once_at_a_time() {
        let mut cache = ChangeCache::default();
        let t = ThreadId::new();
        let generation = cache.claim(t).unwrap();
        assert_eq!(cache.claim(t), None, "already under way");
        assert_eq!(cache.finish(t, generation, files(2), true), None);
        assert_eq!(cache.fresh(t), Some(files(2)));
        // Free again once done.
        assert!(cache.claim(t).is_some());
    }

    #[test]
    fn counts_from_before_a_change_are_not_current() {
        let mut cache = ChangeCache::default();
        let t = ThreadId::new();
        let generation = cache.claim(t).unwrap();
        assert_eq!(cache.finish(t, generation, files(1), true), None);
        let generation = cache.claim(t).unwrap();
        cache.invalidate(t);
        // Worked out again, still claimed.
        let next = cache.finish(t, generation, files(2), true).unwrap();
        assert_eq!(cache.claim(t), None);
        assert_eq!(cache.fresh(t), None);
        assert_eq!(cache.last(t), Some(files(1)));
        assert_eq!(cache.finish(t, next, files(3), true), None);
        assert_eq!(cache.fresh(t), Some(files(3)));
        // Out of tries: released, nothing saved.
        let generation = cache.claim(t).unwrap();
        cache.invalidate(t);
        assert_eq!(cache.finish(t, generation, files(4), false), None);
        assert_eq!(cache.fresh(t), None);
        assert!(cache.claim(t).is_some());
    }

    #[test]
    fn live_edits_drop_counts_at_most_every_five_seconds() {
        let mut cache = ChangeCache::default();
        let t = ThreadId::new();
        let generation = cache.claim(t).unwrap();
        cache.finish(t, generation, files(1), true);
        let start = Instant::now();
        assert_eq!(cache.invalidate_live(t, start), LiveEdit::Dropped);
        assert_eq!(cache.fresh(t), None);
        let generation = cache.claim(t).unwrap();
        cache.finish(t, generation, files(2), true);
        assert_eq!(
            cache.invalidate_live(t, start + Duration::from_secs(4)),
            LiveEdit::Held {
                wait: Duration::from_secs(1),
                first: true
            }
        );
        assert_eq!(
            cache.invalidate_live(t, start + Duration::from_millis(4500)),
            LiveEdit::Held {
                wait: Duration::from_millis(500),
                first: false
            }
        );
        assert_eq!(cache.fresh(t), Some(files(2)));
        // The held back edit drops them once its time comes.
        assert!(!cache.apply_due(t, start + Duration::from_millis(4900)));
        assert_eq!(cache.fresh(t), Some(files(2)));
        assert!(cache.apply_due(t, start + Duration::from_secs(5)));
        assert_eq!(cache.fresh(t), None);
        let generation = cache.claim(t).unwrap();
        cache.finish(t, generation, files(3), true);
        // Nothing held back: nothing to do.
        assert!(!cache.apply_due(t, start + Duration::from_secs(20)));
        assert_eq!(cache.fresh(t), Some(files(3)));
        assert_eq!(
            cache.invalidate_live(t, start + Duration::from_secs(20)),
            LiveEdit::Dropped
        );
        assert_eq!(cache.fresh(t), None);
        // Each thread on its own.
        assert_eq!(
            cache.invalidate_live(ThreadId::new(), start),
            LiveEdit::Dropped
        );
    }

    #[test]
    fn only_edit_calls_count() {
        let mut cache = ChangeCache::default();
        let t = ThreadId::new();
        cache.edit_started(t, "e1".into());
        assert!(!cache.edit_finished(t, "r1"));
        assert!(!cache.edit_finished(ThreadId::new(), "e1"));
        assert!(cache.edit_finished(t, "e1"));
        assert!(!cache.edit_finished(t, "e1"));
    }

    #[test]
    fn the_excerpt_prefers_the_ask_then_the_agent_then_you() {
        assert_eq!(pick_excerpt(Some("ask"), Some("reply"), Some("you")), "ask");
        assert_eq!(pick_excerpt(None, Some("reply"), Some("you")), "reply");
        assert_eq!(pick_excerpt(None, None, Some("you")), "you");
        assert_eq!(pick_excerpt(None, None, None), "");
    }

    #[test]
    fn markdown_becomes_one_plain_line() {
        let md = "## Done\n\n- Fixed **the** `login` bug\n- See [the notes](https://example.com/x)\n\n```rust\nfn main() {}\n```\n> 1. quoted\tline\u{200b}";
        assert_eq!(
            plain_excerpt(md),
            "Done Fixed the login bug See the notes quoted line"
        );
        assert_eq!(
            plain_excerpt("__bold__ and ![logo](a.png)"),
            "bold and logo"
        );
        assert_eq!(plain_excerpt("[not a link"), "[not a link");
    }

    #[test]
    fn a_long_excerpt_is_cut() {
        let long = "word ".repeat(100);
        let cut = plain_excerpt(&long);
        assert_eq!(cut.chars().count(), EXCERPT_LEN);
        assert!(cut.ends_with('…'));
        let short = "é".repeat(EXCERPT_LEN);
        assert_eq!(plain_excerpt(&short), short);
    }
}
