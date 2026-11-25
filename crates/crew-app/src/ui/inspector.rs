//! The right-hand column of an open thread: what a run is doing, what
//! the thread changed and the thread's settings.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crew_core::event::{Event, RunStatus};
use crew_core::ids::{ProjectId, RunId, ThreadId};
use crew_core::store::StoredRun;
use crew_proto::ThreadChanges;
use gpui_kit::component::button::Button;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable as _, Icon, IconName, IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::activity::{ActivityRow, activity_rows_asked};
use super::common::{ROLL_LINE, clock, elapsed, monogram, now_ms, rolling_text};
use super::folder::{FolderMode, folder_mode};
use super::run_card::{FileChange, decided_asks, summarize};
use super::terminal_tab::CommandLog;
use super::workspace::Workspace;
use crate::model::LoggedEvent;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

pub const INSPECTOR_WIDTH: f32 = 400.;
const NO_RUNS: &str = "No runs yet. Mention an agent to start one.";
/// Why Review changes is off.
pub(super) const NOTHING_TO_REVIEW: &str = "No uncommitted changes in this thread.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectorTab {
    #[default]
    Activity,
    Changes,
    Terminal,
    Thread,
}

type RunSelect = SelectState<Vec<SharedString>>;

/// What the inspector shows and keeps between frames.
pub(super) struct Inspector {
    pub(super) tab: InspectorTab,
    /// The run you picked, if any. Otherwise the latest is shown.
    picked: Option<RunId>,
    pub(super) list: ListState,
    /// The shown run's activity.
    pub(super) rows: Vec<ActivityRow>,
    /// The run and event count `rows` were made from.
    pub(super) rows_from: Option<(RunId, usize)>,
    /// Tool calls, by run and call id, whose whole output is shown.
    pub(super) expanded: HashSet<(RunId, String)>,
    /// Each run's changed files and the event count they were made from.
    run_files: HashMap<RunId, (usize, Vec<FileChange>)>,
    /// The thread's changed files, all runs merged.
    pub(super) changes: Vec<FileChange>,
    /// The run picker, once the thread has several runs.
    select: Option<(Entity<RunSelect>, Subscription)>,
    /// The picker's runs and their labels, oldest first.
    choices: Vec<(RunId, SharedString)>,
    /// Uncommitted changes in the open thread's working copy, per repo.
    pub(super) diff: Option<ThreadChanges>,
    /// What `diff` was asked for: thread, worktree and finished runs.
    diff_for: Option<(ThreadId, Option<PathBuf>, usize)>,
    pub(super) diff_task: Option<Task<()>>,
    /// The plain files at the top of a folder of repos, which a thread
    /// folder leaves out, once read.
    pub(super) top_files: Option<(ProjectId, Vec<String>)>,
    top_files_task: Option<Task<()>>,
    /// The shown run's commands, for the Terminal tab.
    pub(super) term: CommandLog,
    pub(super) term_list: ListState,
    /// The run `term` was made from.
    pub(super) term_run: Option<RunId>,
    /// How many of the thread's events `term` has taken in.
    pub(super) term_done: usize,
    /// Whether that run is still live.
    pub(super) term_live: bool,
    /// The thread and the last event (run, seq) `term` has taken in, to
    /// tell a replayed log from the one it was made from.
    pub(super) term_thread: Option<ThreadId>,
    pub(super) term_last: Option<(RunId, u64)>,
    /// Commands, by run and call id, whose whole output is shown.
    pub(super) term_expanded: HashSet<(RunId, String)>,
    /// A command to scroll to once the rows have it.
    pub(super) term_reveal: Option<(RunId, String)>,
}

impl Default for Inspector {
    fn default() -> Self {
        Self {
            tab: InspectorTab::default(),
            picked: None,
            list: activity_list(),
            rows: Vec::new(),
            rows_from: None,
            expanded: HashSet::new(),
            run_files: HashMap::new(),
            changes: Vec::new(),
            select: None,
            choices: Vec::new(),
            diff: None,
            diff_for: None,
            diff_task: None,
            top_files: None,
            top_files_task: None,
            term: CommandLog::default(),
            term_list: activity_list(),
            term_run: None,
            term_done: 0,
            term_live: false,
            term_thread: None,
            term_last: None,
            term_expanded: HashSet::new(),
            term_reveal: None,
        }
    }
}

/// The activity list. It follows new rows while you are at its end.
fn activity_list() -> ListState {
    let list = ListState::new(0, ListAlignment::Top, px(400.));
    list.set_follow_mode(FollowMode::Tail);
    list
}

impl Inspector {
    /// Forgets what was shown for the thread you left.
    pub(super) fn reset(&mut self) {
        let tab = self.tab;
        *self = Self {
            tab,
            ..Self::default()
        };
    }
}

/// `Forge · 10:05`, made unique with a count when two would read alike.
fn run_labels(runs: &[(RunId, String, i64)]) -> Vec<(RunId, SharedString)> {
    let mut seen: Vec<String> = Vec::new();
    runs.iter()
        .map(|(id, name, started)| {
            let base = format!("{name} · {}", clock(*started));
            let n = seen.iter().filter(|s| **s == base).count();
            seen.push(base.clone());
            let label = if n == 0 {
                base
            } else {
                format!("{base} ({})", n + 1)
            };
            (*id, label.into())
        })
        .collect()
}

/// Files changed by several runs, merged by path in the order first
/// touched.
pub fn merge_changes<'a>(runs: impl IntoIterator<Item = &'a [FileChange]>) -> Vec<FileChange> {
    let mut files: Vec<FileChange> = Vec::new();
    for f in runs.into_iter().flatten() {
        match files.iter_mut().find(|known| known.path == f.path) {
            Some(known) => {
                known.added += f.added;
                known.removed += f.removed;
            }
            None => files.push(f.clone()),
        }
    }
    files
}

/// How many events each run has, runs in the order first seen.
fn events_per_run(events: &[LoggedEvent]) -> Vec<(RunId, usize)> {
    let mut counts: Vec<(RunId, usize)> = Vec::new();
    for e in events {
        match counts.iter_mut().find(|(r, _)| *r == e.run) {
            Some((_, n)) => *n += 1,
            None => counts.push((e.run, 1)),
        }
    }
    counts
}

/// The footer's context text for `run`, from its latest `Usage`: the
/// percent used once the window is known, else the tokens in thousands.
/// `None` until the run reported any.
pub(super) fn context_label(events: &[LoggedEvent], run: RunId) -> Option<String> {
    let (tokens, window) = events.iter().rev().find_map(|e| match &e.event {
        Event::Usage {
            context_tokens,
            context_window,
        } if e.run == run => Some((*context_tokens, *context_window)),
        _ => None,
    })?;
    Some(match window.filter(|w| *w > 0) {
        Some(window) => {
            let pct = (u128::from(tokens) * 100 / u128::from(window)).min(100);
            format!("Context {pct}%")
        }
        None => format!("{}k tokens", (tokens + 500) / 1000),
    })
}

/// Where a run is, as a word, its colour token and how long it took.
pub(super) fn run_state(run: &StoredRun) -> (String, &'static str) {
    let took = |end: i64| elapsed(end - run.started_at);
    match run.status {
        RunStatus::Queued => ("Queued".into(), "text-tertiary"),
        RunStatus::Working => (format!("Working · {}", took(now_ms())), "accent-blue"),
        RunStatus::Done => (
            format!("Done · {}", took(run.ended_at.unwrap_or(run.started_at))),
            "success",
        ),
        RunStatus::Failed => ("Failed".into(), "danger"),
        RunStatus::Stopped => (
            format!("Stopped · {}", took(run.ended_at.unwrap_or(run.started_at))),
            "text-secondary",
        ),
        RunStatus::Interrupted => ("Interrupted".into(), "text-secondary"),
    }
}

impl Workspace {
    /// The run the inspector shows: the one you picked, else the latest
    /// live one, else the latest.
    pub(super) fn shown_run(&self, thread: ThreadId) -> Option<&StoredRun> {
        let state = self.state();
        let runs = state.thread_runs(thread);
        if let Some(picked) = self.inspector.picked
            && let Some(run) = runs.iter().find(|r| r.id == picked)
        {
            return Some(run);
        }
        let live = state.live_runs(thread);
        live.last().or(runs.last()).copied()
    }

    /// Brings the inspector in line with what is known: the run picker,
    /// the activity rows and the working copy's changes.
    pub(super) fn sync_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.nav.thread else {
            return;
        };
        let state = self.state();
        let runs: Vec<(RunId, String, i64)> = state
            .thread_runs(thread)
            .iter()
            .map(|r| {
                let name = state
                    .agent(r.agent)
                    .map_or_else(|| "Agent".to_string(), |a| a.spec.name.clone());
                (r.id, name, r.started_at)
            })
            .collect();
        let choices = run_labels(&runs);
        let shown = self.shown_run(thread).map(|r| r.id);
        // A skill run's row needs the skill's file.
        if let Some(project) = shown
            .and_then(|r| state.run(r))
            .filter(|r| r.skill.is_some())
            .and_then(|_| state.thread_project(thread))
            && !self.skills.projects.contains_key(&project)
        {
            self.ensure_skills(project, cx);
        }
        let state = self.state();
        let counts = events_per_run(state.events(thread));
        let approvals = state.thread_approvals(thread);
        let asks = shown
            .map(|run| decided_asks(&approvals, run))
            .unwrap_or_default();
        // An answered ask tags a row, so it counts as a change too.
        let count_of = |run: RunId| {
            counts.iter().find(|(r, _)| *r == run).map_or(0, |c| c.1)
                + if Some(run) == shown { asks.len() } else { 0 }
        };
        let worktree = state
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.path.clone());
        let finished = state
            .thread_runs(thread)
            .iter()
            .filter(|r| r.ended_at.is_some())
            .count();

        self.sync_picker(choices, shown, window, cx);

        match shown {
            Some(run) if self.inspector.rows_from != Some((run, count_of(run))) => {
                let mut rows = self.request_rows(run);
                rows.extend(activity_rows_asked(self.state().events(thread), run, &asks));
                let same_run = self.inspector.rows_from.map(|(r, _)| r) == Some(run);
                let old = std::mem::replace(&mut self.inspector.rows, rows);
                let list = &self.inspector.list;
                if same_run {
                    let from = old
                        .iter()
                        .zip(&self.inspector.rows)
                        .take_while(|(a, b)| a == b)
                        .count();
                    list.splice(from..old.len(), self.inspector.rows.len() - from);
                } else {
                    list.reset(self.inspector.rows.len());
                }
                self.inspector.rows_from = Some((run, count_of(run)));
            }
            Some(_) => {}
            None => {
                if self.inspector.rows_from.is_some() {
                    self.inspector.rows.clear();
                    self.inspector.list.reset(0);
                    self.inspector.rows_from = None;
                }
            }
        }

        self.sync_commands(thread, shown);

        let mut changed = false;
        for (run, n) in &counts {
            if self.inspector.run_files.get(run).map(|f| f.0) != Some(*n) {
                let files = summarize(self.state().events(thread), *run).files;
                self.inspector.run_files.insert(*run, (*n, files));
                changed = true;
            }
        }
        if changed {
            let files = &self.inspector.run_files;
            self.inspector.changes = merge_changes(
                counts
                    .iter()
                    .filter_map(|(run, _)| files.get(run).map(|f| f.1.as_slice())),
            );
        }

        self.read_top_files(thread, cx);

        // Read on every tab: the Done card and Changes need it to offer a
        // review.
        let key = (thread, worktree, finished);
        if self.inspector.diff_for.as_ref() != Some(&key) {
            self.inspector.diff_for = Some(key);
            self.load_thread_diff(thread, cx);
        }
    }

    /// Lists the plain files at the top of the folder of repos the
    /// thread copies, once per thread shown.
    fn read_top_files(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        let state = self.state();
        if folder_mode(state, thread) != Some(FolderMode::OwnCopy) {
            return;
        }
        let Some(project) = state.thread_project(thread) else {
            return;
        };
        if self.inspector.top_files_task.is_some()
            || self
                .inspector
                .top_files
                .as_ref()
                .is_some_and(|(p, _)| *p == project)
        {
            return;
        }
        let Some(root) = state.project(project).map(|p| p.root.clone()) else {
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { plain_top_files(&root) });
        self.inspector.top_files_task = Some(cx.spawn(async move |this, cx| {
            let files = read.await;
            this.update(cx, |this, cx| {
                this.inspector.top_files_task = None;
                this.inspector.top_files = Some((project, files));
                cx.notify();
            })
            .ok();
        }));
    }

    /// Keeps the run picker's items and choice in line.
    fn sync_picker(
        &mut self,
        choices: Vec<(RunId, SharedString)>,
        shown: Option<RunId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if choices.len() < 2 {
            self.inspector.select = None;
            self.inspector.choices = choices;
            return;
        }
        let labels: Vec<SharedString> = choices.iter().map(|(_, l)| l.clone()).collect();
        let shown_label = shown
            .and_then(|run| choices.iter().find(|(r, _)| *r == run))
            .map(|(_, l)| l.clone());
        match &self.inspector.select {
            None => {
                let ix = shown_label
                    .as_ref()
                    .and_then(|l| labels.iter().position(|x| x == l))
                    .map(IndexPath::new);
                let select = cx.new(|cx| SelectState::new(labels, ix, window, cx));
                let events = cx.subscribe_in(&select, window, Self::run_picked);
                self.inspector.select = Some((select, events));
            }
            Some((select, _)) => {
                let select = select.clone();
                let changed = self.inspector.choices != choices;
                select.update(cx, |select, cx| {
                    if changed {
                        select.set_items(labels, window, cx);
                    }
                    if let Some(label) = shown_label
                        && select.selected_value() != Some(&label)
                    {
                        select.set_selected_value(&label, window, cx);
                    }
                });
            }
        }
        self.inspector.choices = choices;
    }

    fn run_picked(
        &mut self,
        _: &Entity<RunSelect>,
        event: &SelectEvent<Vec<SharedString>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SelectEvent::Confirm(Some(label)) = event else {
            return;
        };
        let picked = self
            .inspector
            .choices
            .iter()
            .find(|(_, l)| l == label)
            .map(|(r, _)| *r);
        if picked.is_some() {
            self.inspector.picked = picked;
            self.sync_inspector(window, cx);
            cx.notify();
        }
    }

    /// Shows `run`'s Terminal tab, scrolled to the command `call`, even
    /// when the inspector was hidden.
    pub(super) fn show_run_command(
        &mut self,
        thread: ThreadId,
        run: RunId,
        call: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state().thread_runs(thread).iter().any(|r| r.id == run) {
            self.inspector.picked = Some(run);
        }
        self.inspector.tab = InspectorTab::Terminal;
        self.inspector.term_reveal = Some((run, call));
        self.inspector_open = Some(true);
        self.sync_inspector(window, cx);
        cx.notify();
    }

    pub(super) fn show_tab(
        &mut self,
        tab: InspectorTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inspector.tab = tab;
        if tab == InspectorTab::Thread {
            // Opening the tab always reads the working copy afresh.
            self.inspector.diff_for = None;
        }
        self.sync_inspector(window, cx);
        cx.notify();
    }

    /// The inspector column for the open thread.
    pub(super) fn render_inspector(
        &self,
        thread: ThreadId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let body = match self.inspector.tab {
            InspectorTab::Activity => self.render_activity(thread, cx).into_any_element(),
            InspectorTab::Changes => self.render_changes(thread, cx).into_any_element(),
            InspectorTab::Terminal => self.render_commands(cx).into_any_element(),
            InspectorTab::Thread => self.render_thread_tab(thread, cx).into_any_element(),
        };
        div()
            .id("inspector")
            .flex()
            .flex_col()
            .flex_none()
            .w(scale::chrome(INSPECTOR_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(color("border-100"))
            .child(self.render_inspector_header(thread, cx))
            .child(self.render_tabs(cx))
            .child(div().flex().flex_col().flex_1().min_h_0().child(body))
            .test_support()
    }

    fn render_inspector_header(
        &self,
        thread: ThreadId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let state = self.state();
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(8.))
            .h(scale::row(44.))
            .px(ui_px(16.))
            .border_b_1()
            .border_color(color("border-100"));
        let Some(run) = self.shown_run(thread) else {
            return header.child(
                div()
                    .id("inspector-empty")
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(NO_RUNS)
                    .child(NO_RUNS)
                    .test_support(),
            );
        };
        let name = state
            .agent(run.agent)
            .map_or_else(|| "Agent".to_string(), |a| a.spec.name.clone());
        let (word, token) = run_state(run);
        let live = matches!(run.status, RunStatus::Queued | RunStatus::Working);
        let id = run.id;
        header
            .child(monogram(&name, 24.))
            .child(
                div()
                    .id("inspector-agent")
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .aria_label(name.clone())
                    .child(name)
                    .test_support(),
            )
            .child(
                div()
                    .id("inspector-run-state")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(scale::text(12.))
                    .text_color(color(token))
                    .aria_label(word.clone())
                    .map(|state| {
                        if run.status == RunStatus::Working {
                            // `Working ·` stays still while the time rolls.
                            let ms = now_ms() - run.started_at;
                            // Mono digits keep their width as they change, and
                            // one line height keeps the roll level with the
                            // words.
                            state
                                .flex()
                                .items_center()
                                .gap(ui_px(4.))
                                .line_height(scale::line(ROLL_LINE))
                                .child("Working ·")
                                .child(
                                    rolling_text(
                                        "inspector-elapsed",
                                        &elapsed(ms - 1000),
                                        &elapsed(ms),
                                    )
                                    .font_family(code_family())
                                    .text_size(scale::mono(12.))
                                    .test_support(),
                                )
                        } else {
                            state.child(word)
                        }
                    })
                    .test_support(),
            )
            .when(live, |header| {
                header.child(
                    Button::new("inspector-stop")
                        .outline()
                        .small()
                        .text_color(color("danger"))
                        .icon(IconName::Square)
                        .label("Stop")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.stop_run(id, window, cx);
                        })),
                )
            })
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let changes = self.inspector.changes.len();
        let tab = |id: &'static str, label: &'static str, which: InspectorTab| {
            let selected = self.inspector.tab == which;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(ui_px(6.))
                .h_full()
                .px(ui_px(2.))
                .border_b_1()
                .border_color(if selected {
                    color("primary")
                } else {
                    gpui_kit::transparent_black()
                })
                .text_color(color(if selected {
                    "text-primary"
                } else {
                    "text-secondary"
                }))
                .cursor_pointer()
                .role(Role::Tab)
                .aria_label(label)
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.show_tab(which, window, cx);
                }))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(16.))
            .h(scale::row(34.))
            .px(ui_px(16.))
            .border_b_1()
            .border_color(color("border-100"))
            .text_size(scale::text(12.))
            .child(tab("inspector-tab-activity", "Activity", InspectorTab::Activity).test_support())
            .child(
                tab("inspector-tab-changes", "Changes", InspectorTab::Changes)
                    .when(changes > 0, |tab| {
                        tab.child(
                            div()
                                .px(ui_px(5.))
                                .rounded_full()
                                .bg(color("surface-300"))
                                .text_size(scale::text(11.))
                                .font_weight(weight(FontWeight::MEDIUM))
                                .child(changes.to_string()),
                        )
                    })
                    .test_support(),
            )
            .child(tab("inspector-tab-terminal", "Terminal", InspectorTab::Terminal).test_support())
            .child(tab("inspector-tab-thread", "Thread", InspectorTab::Thread).test_support())
    }

    /// The run picker, once the thread has several runs.
    pub(super) fn render_picker(&self) -> Option<impl IntoElement + use<>> {
        self.inspector.select.as_ref().map(|(select, _)| {
            div()
                .id("inspector-run-picker")
                .flex_none()
                .px(ui_px(16.))
                .py(scale::row(8.))
                .border_b_1()
                .border_color(color("border-100"))
                .child(Select::new(select).small().accessibility_label("Run"))
                .test_support()
        })
    }

    fn render_activity(
        &self,
        thread: ThreadId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let picker = self.render_picker();
        let empty = self.inspector.rows.is_empty();
        let branch = self
            .state()
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.branch.clone());
        let context = self
            .shown_run(thread)
            .and_then(|run| context_label(self.state().events(thread), run.id));
        div()
            .flex()
            .flex_col()
            .size_full()
            .children(picker)
            .child(if empty {
                div()
                    .flex_1()
                    .p(ui_px(16.))
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .child("Nothing yet. What the agent does shows up here.")
                    .into_any_element()
            } else {
                div()
                    .id("activity-list")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            self.inspector.list.clone(),
                            cx.processor(|this, ix, _, cx| this.render_activity_row(ix, cx)),
                        )
                        .size_full(),
                    )
                    .test_support()
                    .into_any_element()
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(ui_px(6.))
                    .h(scale::row(32.))
                    .px(ui_px(16.))
                    .border_t_1()
                    .border_color(color("border-100"))
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .size(ui_px(12.))
                            .text_color(color("text-tertiary")),
                    )
                    .child("Runs your")
                    .child(div().font_family(code_family()).child("claude"))
                    .child("CLI")
                    .when_some(branch, |footer, branch| {
                        footer.child("· branch").child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(code_family())
                                .child(branch),
                        )
                    })
                    .when_some(context, |footer, text| {
                        footer.child(div().flex_1()).child(
                            div()
                                .id("context-usage")
                                .flex_none()
                                .aria_label(text.clone())
                                .child(text)
                                .test_support(),
                        )
                    }),
            )
    }

    fn render_changes(&self, thread: ThreadId, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let can_review = self.can_review(thread);
        let files = self.inspector.changes.clone();
        let added: u32 = files.iter().map(|f| f.added).sum();
        let removed: u32 = files.iter().map(|f| f.removed).sum();
        let n = files.len();
        div()
            .id("changes")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .px(ui_px(16.))
                    .py(scale::row(10.))
                    .text_size(scale::text(12.))
                    .child(
                        div()
                            .flex_1()
                            .text_color(color("text-secondary"))
                            .child(if n == 0 {
                                "No changes yet.".to_string()
                            } else {
                                format!("{n} {} changed", if n == 1 { "file" } else { "files" })
                            }),
                    )
                    .when(n > 0, |row| {
                        row.child(
                            div()
                                .font_family(code_family())
                                .flex()
                                .gap(ui_px(4.))
                                .child(
                                    div()
                                        .text_color(color("success"))
                                        .child(format!("+{added}")),
                                )
                                .child(
                                    div()
                                        .text_color(color("danger"))
                                        .child(format!("−{removed}")),
                                ),
                        )
                    }),
            )
            .children(files.into_iter().enumerate().map(|(ix, f)| {
                div()
                    .id(("change-file", ix))
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .h(scale::row(28.))
                    .px(ui_px(16.))
                    .font_family(code_family())
                    .text_size(scale::mono(12.))
                    .aria_label(f.path.clone())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(color("text-primary"))
                            .child(f.path.clone()),
                    )
                    .child(
                        div()
                            .text_color(color("success"))
                            .child(format!("+{}", f.added)),
                    )
                    .child(
                        div()
                            .text_color(color("danger"))
                            .child(format!("−{}", f.removed)),
                    )
                    .test_support()
            }))
            .child(
                div().px(ui_px(16.)).py(scale::row(12.)).child(
                    Button::new("changes-review")
                        .outline()
                        .small()
                        .label("Review changes")
                        .disabled(!can_review)
                        .when(!can_review, |b| b.tooltip(NOTHING_TO_REVIEW))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_review(thread, window, cx);
                        })),
                ),
            )
            .test_support()
    }
}

/// The files right in `root`, not hidden, sorted. Folders aren't listed.
fn plain_top_files(root: &std::path::Path) -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::{context_label, events_per_run, merge_changes, run_labels};
    use crate::model::LoggedEvent;
    use crate::ui::run_card::FileChange;
    use crew_core::event::Event;
    use crew_core::ids::RunId;

    #[test]
    fn changes_merge_by_path_and_events_count_per_run() {
        let file = |path: &str, added, removed| FileChange {
            path: path.into(),
            added,
            removed,
        };
        let a = [file("a.rs", 1, 2), file("b.rs", 3, 0)];
        let b = [file("b.rs", 1, 1)];
        assert_eq!(
            merge_changes([&a[..], &b[..]]),
            [file("a.rs", 1, 2), file("b.rs", 4, 1)]
        );
        let (r1, r2) = (RunId::new(), RunId::new());
        let at = |run, seq| {
            LoggedEvent::new(
                run,
                seq,
                Event::Raw {
                    line: String::new(),
                },
            )
        };
        let events = [at(r1, 1), at(r2, 1), at(r1, 2)];
        assert_eq!(events_per_run(&events), [(r1, 2), (r2, 1)]);
    }
    #[test]
    fn context_label_reads_the_latest_usage_of_the_run() {
        let (r1, r2) = (RunId::new(), RunId::new());
        let usage = |run, seq, tokens, window| {
            LoggedEvent::new(
                run,
                seq,
                Event::Usage {
                    context_tokens: tokens,
                    context_window: window,
                },
            )
        };
        assert_eq!(context_label(&[], r1), None);
        let events = [
            usage(r1, 1, 41_500, None),
            usage(r2, 1, 1, Some(10)),
            usage(r1, 2, 41_499, None),
        ];
        assert_eq!(context_label(&events, r1).as_deref(), Some("41k tokens"));
        let events = [
            usage(r1, 1, 41_500, None),
            usage(r1, 2, 250_000, Some(1_000_000)),
        ];
        assert_eq!(context_label(&events, r1).as_deref(), Some("Context 25%"));
        // Rounded down, and never over 100.
        let events = [usage(r1, 1, 99_999, Some(100_000))];
        assert_eq!(context_label(&events, r1).as_deref(), Some("Context 99%"));
        let events = [usage(r1, 1, 120_000, Some(100_000))];
        assert_eq!(context_label(&events, r1).as_deref(), Some("Context 100%"));
    }

    #[test]
    fn run_labels_stay_unique() {
        let (a, b, c) = (RunId::new(), RunId::new(), RunId::new());
        let labels = run_labels(&[
            (a, "Forge".into(), 0),
            (b, "Forge".into(), 0),
            (c, "Scout".into(), 0),
        ]);
        let texts: Vec<String> = labels.iter().map(|(_, l)| l.to_string()).collect();
        let at = super::clock(0);
        assert_eq!(
            texts,
            [
                format!("Forge · {at}"),
                format!("Forge · {at} (2)"),
                format!("Scout · {at}")
            ]
        );
    }
}
