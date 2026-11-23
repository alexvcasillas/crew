//! The right-hand column of an open thread: what a run is doing, what
//! the thread changed and the thread's settings.

use std::collections::HashSet;
use std::path::PathBuf;

use crew_core::event::RunStatus;
use crew_core::ids::{RunId, ThreadId};
use crew_core::store::StoredRun;
use crew_core::worktree::Diff;
use gpui_kit::component::button::Button;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable as _, Icon, IconName, IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::activity::{ActivityRow, activity_rows};
use super::common::{clock, elapsed, monogram, now_ms};
use super::run_card::{FileChange, summarize};
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::model::LoggedEvent;
use crate::theme::color;

pub const INSPECTOR_WIDTH: f32 = 400.;
const NO_RUNS: &str = "No runs yet. Mention an agent to start one.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectorTab {
    #[default]
    Activity,
    Changes,
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
    rows_from: Option<(RunId, usize)>,
    /// Tool calls whose whole output is shown.
    pub(super) expanded: HashSet<String>,
    /// The run picker, once the thread has several runs.
    select: Option<(Entity<RunSelect>, Subscription)>,
    /// The picker's runs and their labels, oldest first.
    choices: Vec<(RunId, SharedString)>,
    /// Uncommitted changes in the open thread's working copy.
    pub(super) diff: Option<Diff>,
    /// What `diff` was asked for: thread, worktree and finished runs.
    diff_for: Option<(ThreadId, Option<PathBuf>, usize)>,
    diff_task: Option<Task<()>>,
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
            select: None,
            choices: Vec::new(),
            diff: None,
            diff_for: None,
            diff_task: None,
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

/// Files the thread's runs edited or created, with lines added and
/// removed, merged across runs.
pub fn thread_changes(events: &[LoggedEvent]) -> Vec<FileChange> {
    let mut runs: Vec<RunId> = Vec::new();
    for e in events {
        if !runs.contains(&e.run) {
            runs.push(e.run);
        }
    }
    let mut files: Vec<FileChange> = Vec::new();
    for run in runs {
        for f in summarize(events, run).files {
            match files.iter_mut().find(|known| known.path == f.path) {
                Some(known) => {
                    known.added += f.added;
                    known.removed += f.removed;
                }
                None => files.push(f),
            }
        }
    }
    files
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
        let events_len = state.events(thread).len();
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
            Some(run) if self.inspector.rows_from != Some((run, events_len)) => {
                let rows = activity_rows(self.state().events(thread), run);
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
                self.inspector.rows_from = Some((run, events_len));
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

        if self.inspector.tab == InspectorTab::Thread {
            let key = (thread, worktree, finished);
            if self.inspector.diff_for.as_ref() != Some(&key) {
                self.inspector.diff_for = Some(key);
                self.load_diff(thread, cx);
            }
        }
    }

    /// Asks crewd for the working copy's uncommitted changes.
    fn load_diff(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        let answer = self
            .client
            .request(crew_proto::Request::ThreadDiff { thread });
        self.inspector.diff_task = Some(cx.spawn(async move |this, cx| {
            let diff = answer
                .await
                .and_then(|v| serde_json::from_value::<Diff>(v).map_err(|e| e.to_string()));
            this.update(cx, |this, cx| {
                if this.nav.thread == Some(thread) {
                    // A failed read leaves the counts unknown.
                    this.inspector.diff = diff.ok();
                    cx.notify();
                }
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

    pub(super) fn show_tab(
        &mut self,
        tab: InspectorTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inspector.tab = tab;
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
            InspectorTab::Changes => self.render_changes(thread).into_any_element(),
            InspectorTab::Thread => self.render_thread_tab(thread, cx).into_any_element(),
        };
        div()
            .id("inspector")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(INSPECTOR_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(color("border-100"))
            .child(self.render_inspector_header(thread, cx))
            .child(self.render_tabs(thread, cx))
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
            .gap(px(8.))
            .h(px(44.))
            .px(px(16.))
            .border_b_1()
            .border_color(color("border-100"));
        let Some(run) = self.shown_run(thread) else {
            return header.child(
                div()
                    .id("inspector-empty")
                    .text_size(px(12.))
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
                    .font_weight(FontWeight::SEMIBOLD)
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
                    .text_size(px(12.))
                    .text_color(color(token))
                    .aria_label(word.clone())
                    .child(word)
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

    fn render_tabs(&self, thread: ThreadId, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let changes = thread_changes(self.state().events(thread)).len();
        let tab = |id: &'static str, label: &'static str, which: InspectorTab| {
            let selected = self.inspector.tab == which;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(6.))
                .h_full()
                .px(px(2.))
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
            .gap(px(16.))
            .h(px(34.))
            .px(px(16.))
            .border_b_1()
            .border_color(color("border-100"))
            .text_size(px(12.))
            .child(tab("inspector-tab-activity", "Activity", InspectorTab::Activity).test_support())
            .child(
                tab("inspector-tab-changes", "Changes", InspectorTab::Changes)
                    .when(changes > 0, |tab| {
                        tab.child(
                            div()
                                .px(px(5.))
                                .rounded_full()
                                .bg(color("surface-300"))
                                .text_size(px(11.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(changes.to_string()),
                        )
                    })
                    .test_support(),
            )
            .child(tab("inspector-tab-thread", "Thread", InspectorTab::Thread).test_support())
    }

    fn render_activity(
        &self,
        thread: ThreadId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let picker = self.inspector.select.as_ref().map(|(select, _)| {
            div()
                .id("inspector-run-picker")
                .flex_none()
                .px(px(16.))
                .py(px(8.))
                .border_b_1()
                .border_color(color("border-100"))
                .child(Select::new(select).small().accessibility_label("Run"))
                .test_support()
        });
        let empty = self.inspector.rows.is_empty();
        let branch = self
            .state()
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.branch.clone());
        div()
            .flex()
            .flex_col()
            .size_full()
            .children(picker)
            .child(if empty {
                div()
                    .flex_1()
                    .p(px(16.))
                    .text_size(px(12.))
                    .text_color(color("text-tertiary"))
                    .child("Nothing yet. What the agent does shows up here.")
                    .into_any_element()
            } else {
                div()
                    .id("activity-list")
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
                    .gap(px(6.))
                    .h(px(32.))
                    .px(px(16.))
                    .border_t_1()
                    .border_color(color("border-100"))
                    .text_size(px(12.))
                    .text_color(color("text-tertiary"))
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .size(px(12.))
                            .text_color(color("text-tertiary")),
                    )
                    .child("Runs your")
                    .child(div().font_family(mono_font()).child("claude"))
                    .child("CLI")
                    .when_some(branch, |footer, branch| {
                        footer.child("· branch").child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(mono_font())
                                .child(branch),
                        )
                    }),
            )
    }

    fn render_changes(&self, thread: ThreadId) -> impl IntoElement + use<> {
        let files = thread_changes(self.state().events(thread));
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
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(10.))
                    .text_size(px(12.))
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
                                .font_family(mono_font())
                                .flex()
                                .gap(px(4.))
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
                    .gap(px(8.))
                    .h(px(28.))
                    .px(px(16.))
                    .font_family(mono_font())
                    .text_size(px(12.))
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
                div().px(px(16.)).py(px(12.)).child(
                    Button::new("changes-review")
                        .outline()
                        .small()
                        .label("Review changes")
                        .disabled(true)
                        .tooltip("Coming soon."),
                ),
            )
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use super::run_labels;
    use crew_core::ids::RunId;

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
