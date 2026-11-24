//! What the palette lists: its sections and rows, worked out from what the
//! app knows, what you typed and crewd's latest answer. No drawing here.

use std::collections::HashSet;
use std::ops::Range;

use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_proto::{FileHit, SearchResults, ThreadState};

use super::super::common::runtime_chip;
use crate::model::{State, ThreadStatus};
use crate::recents::Recents;

/// Rows a section shows before "Show all {n}".
pub const MAX_ROWS: usize = 5;
/// Threads in Recent.
pub const RECENT_THREADS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    Recent,
    Projects,
    Threads,
    Channels,
    Agents,
    Files,
    Actions,
}

/// What ↵ on a row does.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Thread(ThreadId),
    Channel(ChannelId),
    Agent(AgentId),
    Project(ProjectId),
    File(FileHit),
    /// The new thread composer in the channel, holding `text` if any.
    NewThread {
        channel: ChannelId,
        text: Option<String>,
    },
    Running,
    StopAll,
    AddProject,
    /// Lists every row of the section.
    ShowAll(SectionKind),
}

/// The mark before a row's text.
#[derive(Debug, Clone, PartialEq)]
pub enum Lead {
    /// A status dot in the colour token.
    Dot(&'static str),
    Hash,
    /// The agent's monogram.
    Agent(String),
    File,
    Plus,
    Pulse,
    Stop,
    Folder,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub target: Target,
    pub label: String,
    /// The part of `label` that matches what you typed, in bytes.
    pub highlight: Option<Range<usize>>,
    /// Grey text after the label.
    pub sub: String,
    /// Paths and branches are mono.
    pub mono: bool,
    /// Only the grey text is mono, as a project's path.
    pub sub_mono: bool,
    pub lead: Lead,
    /// The row's own shortcut.
    pub keys: Option<&'static str>,
    pub danger: bool,
}

impl Row {
    fn new(target: Target, label: impl Into<String>, lead: Lead) -> Row {
        Row {
            target,
            label: label.into(),
            highlight: None,
            sub: String::new(),
            mono: false,
            sub_mono: false,
            lead,
            keys: None,
            danger: false,
        }
    }

    fn sub(mut self, sub: impl Into<String>) -> Row {
        self.sub = sub.into();
        self
    }

    fn keys(mut self, keys: &'static str) -> Row {
        self.keys = Some(keys);
        self
    }

    fn matching(mut self, query: &str) -> Row {
        self.highlight = match_range(&self.label, query);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub kind: SectionKind,
    pub title: String,
    pub rows: Vec<Row>,
}

/// crewd's answer to one search.
#[derive(Debug, Clone, Default)]
pub struct Remote {
    pub query: String,
    pub project: Option<ProjectId>,
    pub results: SearchResults,
}

/// What the palette lists from.
pub struct Inputs<'a> {
    pub state: &'a State,
    pub query: &'a str,
    /// The project searched, `None` for all.
    pub scope: Option<ProjectId>,
    pub recents: &'a Recents,
    /// Where "New thread in #{channel}" goes.
    pub channel: Option<ChannelId>,
    pub remote: Option<&'a Remote>,
    /// Sections whose "Show all" you chose.
    pub expanded: &'a HashSet<SectionKind>,
}

/// The palette's sections, in order, empty ones left out.
pub fn sections(i: &Inputs) -> Vec<Section> {
    let query = i.query.trim();
    let mut out = if query.is_empty() {
        vec![recent(i), actions(i)]
    } else {
        let remote = remote_for(i.remote, query, i.scope).unwrap_or_default();
        let SearchResults {
            threads,
            files,
            totals,
            ..
        } = remote.results;
        let projects = local_projects(i, query);
        let channels = local_channels(i, query);
        let agents = local_agents(i, query);
        vec![
            capped(i, SectionKind::Projects, "Projects", 0, projects, |row| {
                row.matching(query)
            }),
            capped(
                i,
                SectionKind::Threads,
                "Threads",
                totals.threads,
                threads,
                |hit| {
                    thread_row(i, hit.thread, &hit.title, &hit.state, hit.channel).matching(query)
                },
            ),
            capped(i, SectionKind::Channels, "Channels", 0, channels, |row| {
                row.matching(query)
            }),
            capped(i, SectionKind::Agents, "Agents", 0, agents, |row| {
                row.matching(query)
            }),
            capped(
                i,
                SectionKind::Files,
                "Files in worktrees",
                totals.files,
                files,
                |hit| file_row(i, hit).matching(query),
            ),
            typed_actions(i, query),
        ]
    };
    out.retain(|s| !s.rows.is_empty());
    out
}

/// What crewd found for `query` in `scope`: its answer to that search,
/// or, while a longer query is on its way, the rows of a shorter one that
/// still match.
fn remote_for(remote: Option<&Remote>, query: &str, scope: Option<ProjectId>) -> Option<Remote> {
    let remote = remote.filter(|r| r.project == scope)?;
    if remote.query == query {
        return Some(remote.clone());
    }
    if !contains(query, &remote.query) {
        return None;
    }
    let mut kept = remote.clone();
    kept.results.threads.retain(|t| contains(&t.title, query));
    kept.results.files.retain(|f| contains(&f.path, query));
    kept.results.totals.threads = kept.results.threads.len() as u32;
    kept.results.totals.files = kept.results.files.len() as u32;
    Some(kept)
}

/// A section of `items` in rows, at most [`MAX_ROWS`] unless expanded,
/// titled "{title} · {n}". `n` is `total` (crewd's count before its
/// limit) or the items there are, whichever is more.
fn capped<T>(
    i: &Inputs,
    kind: SectionKind,
    title: &str,
    total: u32,
    items: Vec<T>,
    row: impl Fn(T) -> Row,
) -> Section {
    let total = (total as usize).max(items.len());
    let open = i.expanded.contains(&kind);
    let shown = if open { items.len() } else { MAX_ROWS };
    let mut rows: Vec<Row> = items.into_iter().take(shown).map(row).collect();
    if total > rows.len() && !open {
        rows.push(Row::new(
            Target::ShowAll(kind),
            format!("Show all {total}"),
            Lead::None,
        ));
    }
    Section {
        kind,
        title: format!("{title} · {total}"),
        rows,
    }
}

/// Your last threads, channel and agent.
fn recent(i: &Inputs) -> Section {
    let state = i.state;
    let mut rows: Vec<Row> = i
        .recents
        .threads
        .iter()
        .filter_map(|t| state.thread(*t).filter(|t| t.archived_at.is_none()))
        .take(RECENT_THREADS)
        .map(|t| {
            let local = local_state(state, t.id);
            thread_row_in(i, t.id, &t.title, &local, t.channel, false)
        })
        .collect();
    if let Some(project) = i.recents.project.filter(|p| state.project(*p).is_some()) {
        rows.push(project_row(state, project));
    }
    if let Some(channel) = i.recents.channel.and_then(|c| state.channel(c)) {
        rows.push(channel_row(state, channel.id));
    }
    if let Some(agent) = i.recents.agent.and_then(|a| state.agent(a)) {
        rows.push(agent_row(state, agent.id));
    }
    Section {
        kind: SectionKind::Recent,
        title: "Recent".into(),
        rows,
    }
}

fn actions(i: &Inputs) -> Section {
    let state = i.state;
    let mut rows = Vec::new();
    if let Some(channel) = i.channel.and_then(|c| state.channel(c)) {
        rows.push(
            Row::new(
                Target::NewThread {
                    channel: channel.id,
                    text: None,
                },
                format!("New thread in #{}", channel.name),
                Lead::Plus,
            )
            .keys("⌘N"),
        );
    }
    let running = state.working_runs().len();
    let needs = state.needs_you().len();
    rows.push(
        Row::new(Target::Running, "Open Running now", Lead::Pulse)
            .sub(format!("{running} running · {needs} needs you"))
            .keys("⌘⇧R"),
    );
    if !state.running().is_empty() {
        let mut stop = Row::new(Target::StopAll, "Stop all agents", Lead::Stop).keys("⌘⇧.");
        stop.danger = true;
        rows.push(stop);
    }
    rows.push(Row::new(Target::AddProject, "Add project…", Lead::Folder));
    Section {
        kind: SectionKind::Actions,
        title: "Actions".into(),
        rows,
    }
}

fn typed_actions(i: &Inputs, query: &str) -> Section {
    let rows = i
        .channel
        .and_then(|c| i.state.channel(c))
        .map(|channel| {
            Row::new(
                Target::NewThread {
                    channel: channel.id,
                    text: Some(query.to_string()),
                },
                format!("New thread in #{}: “{query}”", channel.name),
                Lead::Plus,
            )
            .keys("⌘N")
        })
        .into_iter()
        .collect();
    Section {
        kind: SectionKind::Actions,
        title: "Actions".into(),
        rows,
    }
}

fn thread_row(
    i: &Inputs,
    thread: ThreadId,
    title: &str,
    state: &ThreadState,
    channel: ChannelId,
) -> Row {
    thread_row_in(i, thread, title, state, channel, i.scope.is_none())
}

/// "Title" + "#general · needs you", led by "project · " when `project`.
fn thread_row_in(
    i: &Inputs,
    thread: ThreadId,
    title: &str,
    thread_state: &ThreadState,
    channel: ChannelId,
    project: bool,
) -> Row {
    let state = i.state;
    let (word, token) = state_word(state, thread_state);
    let channel = state.channel(channel);
    let mut parts = Vec::new();
    if project && let Some(p) = channel.and_then(|c| state.project(c.project)) {
        parts.push(p.name.clone());
    }
    if let Some(c) = channel {
        parts.push(format!("#{}", c.name));
    }
    if !word.is_empty() {
        parts.push(word);
    }
    Row::new(Target::Thread(thread), title, Lead::Dot(token)).sub(parts.join(" · "))
}

/// "# name" + "{project} · {n} active".
fn channel_row(state: &State, channel: ChannelId) -> Row {
    let Some(c) = state.channel(channel) else {
        return Row::new(Target::Channel(channel), "", Lead::Hash);
    };
    let project = state
        .project(c.project)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let active = state
        .threads(channel)
        .iter()
        .filter(|t| !state.live_runs(t.id).is_empty())
        .count();
    Row::new(Target::Channel(channel), c.name.clone(), Lead::Hash)
        .sub(format!("{project} · {active} active"))
}

/// A channel found by typing: "# name" + "{project}".
fn found_channel_row(state: &State, channel: ChannelId) -> Row {
    let project = state
        .channel(channel)
        .and_then(|c| state.project(c.project))
        .map(|p| p.name.clone())
        .unwrap_or_default();
    channel_row(state, channel).sub(project)
}

/// "name" + its folder, `~` for home.
fn project_row(state: &State, project: ProjectId) -> Row {
    let Some(p) = state.project(project) else {
        return Row::new(Target::Project(project), "", Lead::Folder);
    };
    let mut row = Row::new(Target::Project(project), p.name.clone(), Lead::Folder)
        .sub(super::super::title_bar::home_relative(&p.root));
    row.sub_mono = true;
    row
}

/// "Name" + "{role} · {runtime} · {model}".
fn agent_row(state: &State, agent: AgentId) -> Row {
    let Some(a) = state.agent(agent) else {
        return Row::new(Target::Agent(agent), "", Lead::None);
    };
    let name = a.spec.name.clone();
    let sub = [a.spec.role.trim().to_string(), runtime_chip(&a.spec)]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    Row::new(Target::Agent(agent), name.clone(), Lead::Agent(name)).sub(sub)
}

/// The path, and the branch it is on, " · changed" when uncommitted,
/// led by the project's name when searching all of them.
fn file_row(i: &Inputs, hit: FileHit) -> Row {
    let state = i.state;
    let branch = match hit.thread {
        Some(t) => state
            .thread(t)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.branch.clone())
            .unwrap_or_default(),
        None => state
            .project(hit.project)
            .map(|p| p.main_branch.clone())
            .unwrap_or_default(),
    };
    let mut sub = file_sub(&branch, hit.changed);
    if i.scope.is_none()
        && let Some(p) = state.project(hit.project)
    {
        sub = format!("{} · {sub}", p.name);
    }
    let mut row = Row::new(Target::File(hit.clone()), hit.path.clone(), Lead::File).sub(sub);
    row.mono = true;
    row
}

/// "crew/fix-login · changed".
pub fn file_sub(branch: &str, changed: bool) -> String {
    if changed {
        format!("{branch} · changed")
    } else {
        branch.to_string()
    }
}

fn local_channels(i: &Inputs, query: &str) -> Vec<Row> {
    let state = i.state;
    let mut hits: Vec<(u8, String, ChannelId)> = state
        .projects()
        .into_iter()
        .filter(|p| i.scope.is_none_or(|s| s == p.id))
        .flat_map(|p| {
            state
                .channels(p.id)
                .into_iter()
                .map(move |c| (format!("{} {}", p.name, c.name), c))
        })
        .filter_map(|(both, c)| {
            let best = [rank(&c.name, query), rank(&both, query)]
                .into_iter()
                .flatten()
                .min()?;
            Some((best, c.name.clone(), c.id))
        })
        .collect();
    hits.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    hits.into_iter()
        .map(|(_, _, id)| found_channel_row(state, id))
        .collect()
}

/// Every project whose name matches, whatever the scope.
fn local_projects(i: &Inputs, query: &str) -> Vec<Row> {
    let state = i.state;
    let mut hits: Vec<(u8, String, ProjectId)> = state
        .projects()
        .into_iter()
        .filter_map(|p| rank(&p.name, query).map(|r| (r, p.name.to_lowercase(), p.id)))
        .collect();
    hits.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    hits.into_iter()
        .map(|(_, _, id)| project_row(state, id))
        .collect()
}

fn local_agents(i: &Inputs, query: &str) -> Vec<Row> {
    let state = i.state;
    let mut hits: Vec<(u8, String, AgentId)> = state
        .agents()
        .into_iter()
        .filter_map(|a| rank(&a.spec.name, query).map(|r| (r, a.spec.name.to_lowercase(), a.id)))
        .collect();
    hits.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    hits.into_iter()
        .map(|(_, _, id)| agent_row(state, id))
        .collect()
}

/// Where a thread stands as the app knows it now.
pub fn local_state(state: &State, thread: ThreadId) -> ThreadState {
    let live = state.live_runs(thread);
    let first = |status: RunStatus| live.iter().find(|r| r.status == status).map(|r| r.agent);
    match state.thread_status(thread) {
        ThreadStatus::NeedsYou => ThreadState::NeedsYou,
        ThreadStatus::Working => first(RunStatus::Working)
            .map_or(ThreadState::Idle, |agent| ThreadState::Working { agent }),
        ThreadStatus::Queued => first(RunStatus::Queued)
            .map_or(ThreadState::Idle, |agent| ThreadState::Queued { agent }),
        ThreadStatus::Idle => ThreadState::Idle,
    }
}

/// "needs you", "{Agent} working" or nothing, and the dot's colour token.
/// The dot is grey whenever there is no word.
fn state_word(state: &State, thread: &ThreadState) -> (String, &'static str) {
    match thread {
        ThreadState::NeedsYou => ("needs you".into(), "warning"),
        ThreadState::Working { agent } => {
            let name = state
                .agent(*agent)
                .map_or_else(|| "An agent".to_string(), |a| a.spec.name.clone());
            (format!("{name} working"), "accent-blue")
        }
        _ => (String::new(), "text-tertiary"),
    }
}

/// Whether `text` holds `needle`, ignoring case.
fn contains(text: &str, needle: &str) -> bool {
    match_range(text, needle).is_some()
}

/// How well `name` matches `query`, ignoring case: 0 starts with it, 1
/// has a word starting with it, 2 holds it. A query of several words
/// that isn't held whole matches when each word is, ranked by its worst
/// word. `None` if it doesn't match.
pub fn rank(name: &str, query: &str) -> Option<u8> {
    if let Some(whole) = rank_whole(name, query) {
        return Some(whole);
    }
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.len() < 2 {
        return None;
    }
    words
        .iter()
        .map(|w| rank_whole(name, w))
        .collect::<Option<Vec<u8>>>()?
        .into_iter()
        .max()
}

fn rank_whole(name: &str, query: &str) -> Option<u8> {
    let at = match_range(name, query)?.start;
    if at == 0 {
        return Some(0);
    }
    let lower = name.to_lowercase();
    let needle = query.to_lowercase();
    let word_start = lower.match_indices(&needle).any(|(i, _)| {
        lower[..i]
            .chars()
            .last()
            .is_none_or(|c| !c.is_alphanumeric())
    });
    Some(if word_start { 1 } else { 2 })
}

/// Where `needle` first shows in `text`, ignoring case, as a byte range
/// of `text`. `None` for an empty needle or no match.
pub fn match_range(text: &str, needle: &str) -> Option<Range<usize>> {
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return None;
    }
    text.char_indices().find_map(|(from, _)| {
        let mut matched = 0;
        for (at, c) in text[from..].char_indices() {
            for lower in c.to_lowercase() {
                if needle.get(matched) != Some(&lower) {
                    return None;
                }
                matched += 1;
            }
            if matched == needle.len() {
                return Some(from..from + at + c.len_utf8());
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::{file_sub, match_range, rank};

    #[test]
    fn matches_ignore_case_and_give_byte_ranges() {
        assert_eq!(match_range("Retry typos", "retry"), Some(0..5));
        assert_eq!(
            match_range("How is the retry backoff", "RETRY"),
            Some(11..16)
        );
        assert_eq!(match_range("café Retry", "retry"), Some(6..11));
        assert_eq!(match_range("docs/retries.mdx", "retri"), Some(5..10));
        assert_eq!(match_range("abc", ""), None);
        assert_eq!(match_range("abc", "abcd"), None);
        assert_eq!(match_range("abc", "x"), None);
    }

    #[test]
    fn a_start_beats_a_word_start_beats_inside() {
        assert_eq!(rank("bugs", "bu"), Some(0));
        assert_eq!(rank("fix-bugs", "bu"), Some(1));
        assert_eq!(rank("Big Build", "bu"), Some(1));
        assert_eq!(rank("rebuild", "bu"), Some(2));
        assert_eq!(rank("general", "bu"), None);
        assert_eq!(rank("supapick general", "supa gen"), Some(1), "each word");
        assert_eq!(rank("supapick general", "pick gen"), Some(2));
        assert_eq!(rank("supapick general", "supa bugs"), None);
        assert_eq!(rank("general", "gen eral"), Some(2));
    }

    #[test]
    fn changed_files_say_so_after_their_branch() {
        assert_eq!(file_sub("main", false), "main");
        assert_eq!(
            file_sub("crew/fix-the-login", true),
            "crew/fix-the-login · changed"
        );
    }
}
