//! A channel at a glance: stats and one summary per thread.
//!
//! Change counts come from git, so they are cached per thread and only
//! worked out again once something may have changed them: a run ended, a
//! commit, a discard or a worktree setup.

use std::collections::HashMap;

use crew_core::event::RunStatus;
use crew_core::ids::{ChannelId, ThreadId};
use crew_core::store::{StoredRun, StoredThread, ThreadActivity};
use crew_core::text::clean_line;
use crew_core::worktree::{self, ChangeCounts};
use crew_proto::{ChannelOverview, ChannelStats, RunBar, ThreadSummary, WorktreeState};
use tokio::task::JoinSet;

use crate::service::{Reply, Service, json, log, now_ms};

/// Longest excerpt, in characters.
pub const EXCERPT_LEN: usize = 240;
/// What "this week" covers.
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Threads whose counts are worked out at once.
const GIT_AT_ONCE: usize = 8;

/// Change counts per thread. Each thread has a generation that grows when
/// its counts may have changed, so counts worked out before that are
/// never saved over the change.
#[derive(Default)]
pub(crate) struct ChangeCache {
    generation: HashMap<ThreadId, u64>,
    counts: HashMap<ThreadId, (u64, ChangeCounts)>,
}

impl ChangeCache {
    fn generation(&self, thread: ThreadId) -> u64 {
        self.generation.get(&thread).copied().unwrap_or(0)
    }

    /// The counts, if worked out since the last change.
    fn get(&self, thread: ThreadId) -> Option<ChangeCounts> {
        let (generation, counts) = self.counts.get(&thread)?;
        (*generation == self.generation(thread)).then_some(*counts)
    }

    /// Saves counts worked out at `generation`, unless the thread changed
    /// since.
    fn put(&mut self, thread: ThreadId, generation: u64, counts: ChangeCounts) {
        if generation == self.generation(thread) {
            self.counts.insert(thread, (generation, counts));
        }
    }

    /// Forgets the thread's counts.
    pub(crate) fn invalidate(&mut self, thread: ThreadId) {
        *self.generation.entry(thread).or_default() += 1;
        self.counts.remove(&thread);
    }
}

/// A thread whose counts must be worked out.
struct Stale {
    thread: ThreadId,
    generation: u64,
    path: std::path::PathBuf,
    base: String,
}

impl Service {
    /// Answers `ChannelOverview`. Git only runs for threads whose counts
    /// are not cached, a few at a time, and a git failure only zeroes that
    /// thread's counts.
    pub(crate) async fn channel_overview(&self, channel: ChannelId) -> Reply {
        let (threads, activity, runs, copied) = {
            let db = self.db();
            let found = db.store.channel(channel).and_then(|c| {
                Ok((
                    db.store.threads(channel)?,
                    db.store.thread_activity(channel)?,
                    db.store.channel_runs(channel)?,
                    db.store.project(c.project)?.copy_files,
                ))
            });
            found.map_err(|e| e.to_string())?
        };

        let mut counts = HashMap::new();
        let mut stale = Vec::new();
        {
            let cache = self.changes();
            for thread in &threads {
                let Some(wt) = &thread.worktree else { continue };
                match cache.get(thread.id) {
                    Some(c) => {
                        counts.insert(thread.id, c);
                    }
                    None => stale.push(Stale {
                        thread: thread.id,
                        generation: cache.generation(thread.id),
                        path: wt.path.clone(),
                        base: wt.base.clone(),
                    }),
                }
            }
        }
        for batch in stale.chunks(GIT_AT_ONCE) {
            let mut set = JoinSet::new();
            for s in batch {
                let (path, base, copied) = (s.path.clone(), s.base.clone(), copied.clone());
                let (thread, generation) = (s.thread, s.generation);
                set.spawn_blocking(move || {
                    (
                        thread,
                        generation,
                        count_changes(thread, &path, &base, &copied),
                    )
                });
            }
            while let Some(done) = set.join_next().await {
                let Ok((thread, generation, c)) = done else {
                    log("a change count task ended early");
                    continue;
                };
                self.changes().put(thread, generation, c);
                counts.insert(thread, c);
            }
        }

        // A folder deleted outside Crew since the counts were cached.
        for thread in &threads {
            if let Some(wt) = &thread.worktree
                && !wt.path.is_dir()
            {
                counts.insert(thread.id, ChangeCounts::default());
            }
        }

        json(summarise(&threads, &activity, &runs, &counts, now_ms()))
    }
}

/// The thread's change counts. A missing folder or a git failure gives
/// zeros; a failure is logged.
fn count_changes(
    thread: ThreadId,
    path: &std::path::Path,
    base: &str,
    copied: &[String],
) -> ChangeCounts {
    if !path.is_dir() {
        return ChangeCounts::default();
    }
    worktree::change_counts(path, base, copied).unwrap_or_else(|e| {
        log(format!(
            "couldn't count the changes of thread {thread}: {e}"
        ));
        ChangeCounts::default()
    })
}

/// How long a run ran, or has run so far. A queued run, or one that ended
/// without starting its CLI, counts 0.
fn run_ms(run: &StoredRun, now: i64) -> i64 {
    match (&run.status, run.ended_at) {
        (RunStatus::Queued, _) => 0,
        (_, Some(_)) if run.session_id.is_empty() => 0,
        (_, Some(ended)) => (ended - run.started_at).max(0),
        (_, None) => (now - run.started_at).max(0),
    }
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
                let this_week = r.status == RunStatus::Working
                    || (r.ended_at.is_some_and(|e| e >= since) && !r.session_id.is_empty());
                if this_week {
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
    use crate::overview::{EXCERPT_LEN, pick_excerpt, plain_excerpt};

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
