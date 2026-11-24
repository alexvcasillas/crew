//! Running now: every agent in every project, in place of the body, with
//! stop controls. Groups are Needs you, Working, In terminal, Queued and
//! Finished in the last hour.

use std::collections::HashSet;

use crew_core::agent::Runtime;
use crew_core::claude::retry_rule;
use crew_core::event::{RunStatus, tool_label};
use crew_core::ids::{ApprovalId, ProjectId, RunId, ThreadId};
use crew_core::store::{ApprovalKind, StoredApproval, StoredRun};
use crew_proto::TerminalSession;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::approval_card::input_text;
use super::common::{
    chip, cut_start, dot, elapsed, monogram, now_ms, rolling_text, runtime_badge_for, runtime_chip,
    spinner, status_word,
};
use super::inspector::run_state;
use super::run_card::summarize;
use super::workspace::{SIDEBAR_WIDTH, Workspace};
use crate::fonts::mono_font;
use crate::model::{RunningNow, ThreadStatus};
use crate::theme::color;

actions!(
    running,
    [
        /// Leave Running now.
        CloseRunning,
    ]
);

const RUNNING_CONTEXT: &str = "Running";
/// Column widths of a row: agent, (task), runtime, status, actions.
const AGENT_COL: f32 = 200.;
const RUNTIME_COL: f32 = 150.;
const STATUS_COL: f32 = 110.;
const ACTIONS_COL: f32 = 190.;
const ROW_MIN_HEIGHT: f32 = 56.;
const COMING_SOON: &str = "Coming soon.";

/// Esc leaves Running now.
pub fn bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new(
        "escape",
        CloseRunning,
        Some(RUNNING_CONTEXT),
    )]
}

/// What Running now shows.
pub(super) struct RunningView {
    /// Only this project's rows, if set.
    pub(super) project: Option<ProjectId>,
    /// Only the Needs you group.
    pub(super) needs_only: bool,
    pub(super) focus: FocusHandle,
}

impl RunningView {
    pub(super) fn new(focus: FocusHandle) -> Self {
        Self {
            project: None,
            needs_only: false,
            focus,
        }
    }
}

/// "3 agents working across 2 projects. 1 is waiting on you."
pub fn summary_line(working: usize, projects: usize, waiting: usize) -> String {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("{n} {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let waits = if waiting == 1 { "is" } else { "are" };
    match (working, waiting) {
        (0, 0) => "Nothing is running.".into(),
        (0, n) => format!("{} {waits} waiting on you.", plural(n, "agent", "agents")),
        (w, n) => {
            let line = format!(
                "{} working across {}.",
                plural(w, "agent", "agents"),
                plural(projects, "project", "projects")
            );
            if n == 0 {
                line
            } else {
                format!("{line} {n} {waits} waiting on you.")
            }
        }
    }
}

/// Which group a row is in, and what its actions act on.
#[derive(Clone, Copy)]
enum RowKind {
    Ask {
        id: ApprovalId,
        handoff: bool,
        paused: bool,
        /// A call the safety check stopped, to retry or dismiss.
        blocked: bool,
        /// Whether a stopped call can be retried exactly. Else it can only
        /// be dismissed.
        retryable: bool,
    },
    Working(RunId),
    Queued(RunId),
    Finished(RunId),
}

/// One row, owned, so it renders without holding the state.
struct Row {
    kind: RowKind,
    thread: ThreadId,
    agent: String,
    project: String,
    channel: String,
    title: String,
    /// What the agent is doing now.
    now: String,
    now_mono: bool,
    chip: String,
    /// The runtime behind `chip`, for its provider mark.
    runtime: Option<Runtime>,
    state: String,
    /// A working run's time a second ago, so the new one rolls in.
    rolling_from: Option<String>,
    state_token: &'static str,
    state_mono: bool,
    /// Your answer to the row's approval was sent.
    deciding: bool,
}

impl Workspace {
    /// Shows Running now in place of the body.
    pub(super) fn show_running(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_review(cx);
        self.settings = None;
        let view = self
            .running
            .take()
            .unwrap_or_else(|| RunningView::new(cx.focus_handle()));
        view.focus.focus(window, cx);
        self.running = Some(view);
        self.sync(cx);
        cx.notify();
    }

    /// Goes back to what was open before.
    pub(super) fn close_running(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.take().is_none() {
            return;
        }
        if self.open_thread_id().is_some() {
            self.composer
                .update(cx, |composer, cx| composer.focus(window, cx));
        } else {
            self.focus_workspace(window, cx);
        }
        self.sync(cx);
        cx.notify();
    }

    /// Leaves Running now for `thread`.
    fn open_from_running(&mut self, thread: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        self.running = None;
        self.open_thread(thread, window, cx);
    }

    fn filter_project(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
        if let Some(view) = self.running.as_mut() {
            view.project = if view.project == project {
                None
            } else {
                project
            };
            cx.notify();
        }
    }

    fn show_needs_only(&mut self, needs_only: bool, cx: &mut Context<Self>) {
        if let Some(view) = self.running.as_mut() {
            view.needs_only = needs_only;
            cx.notify();
        }
    }

    /// Working runs that don't wait on you, and pending approvals.
    /// Working runs, even those waiting on you, and pending asks.
    pub(super) fn run_counts(&self) -> (usize, usize) {
        let state = self.state();
        (state.working_runs().len(), state.needs_you().len())
    }

    /// Whether the app is connected to crewd now.
    pub(super) fn connected(&self) -> bool {
        self.state().connection() == Some(&crate::model::ConnectionState::Connected)
    }

    /// Whether any run is queued or working.
    pub(super) fn anything_live(&self) -> bool {
        !self.state().running().is_empty()
    }

    /// Whether a Stop all button works now, and its text colour.
    pub(super) fn stop_all_look(&self) -> (bool, &'static str) {
        stop_all_look(self.anything_live(), self.stopping_all)
    }

    /// The title bar's content on Running now.
    pub(super) fn render_running_title(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(AnyElement, AnyElement)> {
        self.running.as_ref()?;
        let left = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Crew"),
            )
            .child(div().flex_none().text_color(rgb(0x3a3a3a)).child("/"))
            .child(
                div()
                    .id("running-title")
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .aria_label("Running now")
                    .child("Running now")
                    .test_support(),
            )
            .into_any_element();
        let right = div()
            .flex()
            .items_center()
            .gap(px(8.))
            // Like the status bar's, only while connected.
            .children(self.connected().then(|| {
                let (enabled, token) = self.stop_all_look();
                Button::new("running-stop-all")
                    .when(enabled, |b| b.outline())
                    .when(!enabled, |b| b.ghost())
                    .small()
                    .text_color(color(token))
                    .icon(IconName::Square)
                    .label("Stop all ⌘⇧.")
                    .disabled(!enabled)
                    .on_click(cx.listener(|this, _, window, cx| this.stop_all(window, cx)))
            }))
            .children(self.render_title_actions(cx))
            .into_any_element();
        Some((left, right))
    }

    /// Running now in place of the body row.
    pub(super) fn render_running(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = &self.running else {
            return div().into_any_element();
        };
        div()
            .id("running")
            .key_context(RUNNING_CONTEXT)
            .track_focus(&view.focus)
            .on_action(cx.listener(|this, _: &CloseRunning, window, cx| {
                this.close_running(window, cx);
            }))
            .flex()
            .flex_1()
            .min_h_0()
            .child(self.render_running_nav(view, cx))
            .child(self.render_running_main(view, cx))
            .test_support()
            .into_any_element()
    }

    fn render_running_nav(&self, view: &RunningView, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let all = state.running_now(None, now_ms());
        let live = all.working.len();
        let needs = all.needs_you.len();
        let projects: Vec<AnyElement> = state
            .projects()
            .into_iter()
            .map(|p| {
                let id = p.id;
                let selected = view.project == Some(id);
                nav_row(format!("running-project-{id}"), selected)
                    .aria_label(p.name.clone())
                    .child(div().flex_1().min_w_0().truncate().child(p.name.clone()))
                    .when_some(status_dot(state.project_status(id)), |row, dot| {
                        row.child(dot)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.filter_project(Some(id), cx);
                    }))
                    .test_support()
                    .into_any_element()
            })
            .collect();
        div()
            .id("running-nav")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .px(px(8.))
            .py(px(12.))
            .border_r_1()
            .border_color(color("border-100"))
            .overflow_y_scroll()
            .child(
                nav_row("running-nav-all".into(), !view.needs_only)
                    .aria_label(format!("Running now {live}"))
                    .child(div().flex_1().child("Running now"))
                    .child(count(live.to_string(), "text-secondary"))
                    .on_click(cx.listener(|this, _, _, cx| this.show_needs_only(false, cx)))
                    .test_support(),
            )
            .child(
                nav_row("running-nav-needs".into(), view.needs_only)
                    .aria_label(format!("Needs you {needs}"))
                    .child(div().flex_1().child("Needs you"))
                    .when(needs > 0, |row| {
                        row.child(count(needs.to_string(), "warning"))
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.show_needs_only(true, cx)))
                    .test_support(),
            )
            .child(
                div()
                    .id("running-nav-history")
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .px(px(8.))
                    .text_color(color("text-tertiary"))
                    .aria_label("History")
                    .child("History")
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(COMING_SOON).build(window, cx)
                    })
                    .test_support(),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .mt(px(16.))
                    .pl(px(8.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color("text-secondary"))
                    .child("Projects"),
            )
            .child(
                nav_row("running-all-projects".into(), view.project.is_none())
                    .aria_label("All projects")
                    .child("All projects")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(view) = this.running.as_mut() {
                            view.project = None;
                        }
                        cx.notify();
                    }))
                    .test_support(),
            )
            .children(projects)
    }

    fn render_running_main(&self, view: &RunningView, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let now = now_ms();
        let groups = state.running_now(view.project, now);
        let projects: HashSet<ProjectId> = groups
            .working
            .iter()
            .filter_map(|r| state.thread_project(r.thread))
            .collect();
        let summary = summary_line(groups.working.len(), projects.len(), groups.needs_you.len());
        let rows = self.running_rows(&groups, now);
        let mut main = div()
            .id("running-main")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px(px(32.))
            .py(px(24.))
            .child(
                div()
                    .text_size(px(20.))
                    .line_height(px(28.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Running now"),
            )
            .child(
                div()
                    .id("running-summary")
                    .mt(px(4.))
                    .mb(px(16.))
                    .text_color(color("text-secondary"))
                    .aria_label(summary.clone())
                    .child(summary)
                    .test_support(),
            );
        let [needs, working, queued, finished] = rows;
        let shown: [(&str, &str, &str, Vec<Row>); 4] = [
            ("needs", "NEEDS YOU", "warning", needs),
            ("working", "WORKING", "accent-blue", working),
            ("queued", "QUEUED", "text-tertiary", queued),
            ("finished", "FINISHED IN THE LAST HOUR", "success", finished),
        ];
        for (key, title, token, rows) in shown {
            // Claude Code at work in a terminal, after Crew's own runs.
            if key == "queued" && !view.needs_only {
                let sessions: Vec<&TerminalSession> = state
                    .live_terminal_sessions()
                    .into_iter()
                    .filter(|s| view.project.is_none_or(|p| p == s.project))
                    .collect();
                if !sessions.is_empty() {
                    main = main.child(group_header(
                        "terminal",
                        "IN TERMINAL",
                        "accent-blue",
                        sessions.len(),
                    ));
                    for session in sessions {
                        let project = state
                            .project(session.project)
                            .map_or_else(String::new, |p| p.name.clone());
                        main = main.child(render_terminal_row(session, project, cx));
                    }
                }
            }
            if rows.is_empty() || (view.needs_only && key != "needs") {
                continue;
            }
            main = main.child(group_header(key, title, token, rows.len()));
            for row in rows {
                main = main.child(render_row(row, cx));
            }
        }
        main
    }

    /// The rows of each group: needs you, working, queued, finished.
    fn running_rows(&self, groups: &RunningNow<'_>, now: i64) -> [Vec<Row>; 4] {
        let asks = groups.needs_you.iter().map(|a| self.ask_row(a)).collect();
        let runs = |runs: &[&StoredRun], kind: fn(RunId) -> RowKind| {
            runs.iter()
                .map(|r| self.run_row(r, kind(r.id), now))
                .collect::<Vec<_>>()
        };
        [
            asks,
            runs(&groups.working, RowKind::Working),
            runs(&groups.queued, RowKind::Queued),
            runs(&groups.finished, RowKind::Finished),
        ]
    }

    /// A row's agent, place, task and runtime. Every row is drawn, so
    /// what the app doesn't know reads as a placeholder and the counts
    /// always match the rows.
    fn row_base(&self, thread: ThreadId, run: Option<&StoredRun>, kind: RowKind) -> Row {
        let state = self.state();
        let known = state.thread(thread);
        let channel = known.and_then(|t| state.channel(t.channel));
        let project = channel.and_then(|c| state.project(c.project));
        let agent = run.and_then(|r| state.agent(r.agent));
        Row {
            kind,
            thread,
            agent: agent.map_or_else(|| "An agent".into(), |a| a.spec.name.clone()),
            project: project.map_or_else(String::new, |p| p.name.clone()),
            channel: channel.map_or_else(String::new, |c| c.name.clone()),
            title: known.map_or_else(String::new, |t| t.title.clone()),
            now: String::new(),
            now_mono: false,
            chip: agent.map_or_else(String::new, |a| runtime_chip(&a.spec)),
            runtime: agent.map(|a| a.spec.runtime.clone()),
            state: String::new(),
            rolling_from: None,
            state_token: "text-secondary",
            state_mono: false,
            deciding: false,
        }
    }

    fn ask_row(&self, approval: &StoredApproval) -> Row {
        let run = self.state().run(approval.run);
        let blocked = matches!(approval.kind, ApprovalKind::Blocked { .. });
        let retryable = match &approval.kind {
            ApprovalKind::Blocked { tool, input, .. } => retry_rule(tool, input).is_some(),
            _ => true,
        };
        let (handoff, paused, now, mono) = match &approval.kind {
            ApprovalKind::Handoff { to, paused, .. } => {
                let to = self
                    .state()
                    .agent(*to)
                    .map_or_else(|| "an agent".to_string(), |a| a.spec.name.clone());
                (true, *paused, format!("Wants to bring in {to}"), false)
            }
            ApprovalKind::Permission { tool, input, .. }
            | ApprovalKind::Blocked { tool, input, .. } => {
                let text = input_text(input);
                let first = text.lines().next().unwrap_or_default();
                (false, false, format!("{}: {first}", tool_label(tool)), true)
            }
        };
        let mut row = self.row_base(
            approval.thread,
            run,
            RowKind::Ask {
                id: approval.id,
                handoff,
                paused,
                blocked,
                retryable,
            },
        );
        row.now = now;
        row.now_mono = mono;
        row.state = if handoff {
            "Handoff"
        } else if blocked {
            "Blocked"
        } else {
            "Permission"
        }
        .into();
        row.state_token = "warning";
        row.deciding = self.deciding.contains(&approval.id);
        row
    }

    fn run_row(&self, run: &StoredRun, kind: RowKind, now: i64) -> Row {
        let mut row = self.row_base(run.thread, Some(run), kind);
        let state = self.state();
        // Watched threads know their latest step; the rest only their
        // status.
        let step = state
            .is_watched(run.thread)
            .then(|| summarize(state.events(run.thread), run.id).steps.pop())
            .flatten()
            .map(|s| s.text());
        let step_shown = step.is_some() && !matches!(kind, RowKind::Queued(_));
        match kind {
            RowKind::Working(_) => {
                row.state = elapsed(now - run.started_at);
                row.rolling_from = Some(elapsed(now - run.started_at - 1000));
                row.state_token = "accent-blue";
                row.state_mono = true;
                row.now = step.unwrap_or_else(|| "Working".into());
            }
            RowKind::Queued(_) => {
                row.state = "Queued".into();
                row.state_token = "text-tertiary";
                row.now = queued_line(
                    state
                        .live_runs(run.thread)
                        .into_iter()
                        .find(|r| r.status == RunStatus::Working)
                        .and_then(|r| state.agent(r.agent))
                        .map(|a| a.spec.name.as_str()),
                );
            }
            _ => {
                let (word, token) = run_state(run);
                row.now = step.unwrap_or_else(|| ended_word(&run.status).into());
                row.state = word;
                row.state_token = token;
            }
        }
        // Steps name paths and commands.
        row.now_mono = step_shown;
        row
    }
}

/// Stop all is red and works only while something runs and no Stop all
/// is waiting for its answer. Otherwise it is tertiary and disabled.
pub fn stop_all_look(live: bool, stopping: bool) -> (bool, &'static str) {
    if live && !stopping {
        (true, "danger")
    } else {
        (false, "text-tertiary")
    }
}

/// Why a queued run waits: another run in its thread, or the cap on
/// runs at once.
pub fn queued_line(busy_in_thread: Option<&str>) -> String {
    match busy_in_thread {
        Some(agent) => format!("Starts after {agent} finishes in this thread"),
        None => "Starts when a run slot is free".into(),
    }
}

/// How a finished run ended, as the line under its task.
fn ended_word(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Done => "Done",
        RunStatus::Failed => "Failed",
        RunStatus::Stopped => "Stopped",
        RunStatus::Interrupted => "Interrupted",
        RunStatus::Queued => "Queued",
        RunStatus::Working => "Working",
    }
}

fn nav_row(id: String, selected: bool) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(28.))
        .px(px(8.))
        .rounded(px(6.))
        .cursor_pointer()
        .when(selected, |row| row.bg(color("surface-300")))
        .text_color(color(if selected {
            "text-primary"
        } else {
            "text-secondary"
        }))
        .hover(|row| row.bg(color("surface-200")))
}

fn count(text: String, token: &str) -> impl IntoElement {
    div()
        .flex_none()
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color(token))
        .child(text)
}

/// A dot for urgent states only: queued runs get none.
fn status_dot(status: ThreadStatus) -> Option<Div> {
    match status {
        ThreadStatus::Idle | ThreadStatus::Queued => None,
        status => Some(dot(status_word(status).1)),
    }
}

fn group_header(key: &str, title: &str, token: &str, n: usize) -> impl IntoElement {
    let text = format!("{title} · {n}");
    div()
        .id(SharedString::from(format!("running-group-{key}")))
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(36.))
        .mt(px(8.))
        .border_b_1()
        .border_color(color("border-100"))
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color("text-tertiary"))
        .aria_label(text.clone())
        .child(dot(token))
        .child(text)
        .test_support()
}

fn render_row(row: Row, cx: &mut Context<Workspace>) -> impl IntoElement {
    let (id, ask) = match row.kind {
        RowKind::Ask { id, .. } => (id.to_string(), true),
        RowKind::Working(run) | RowKind::Queued(run) | RowKind::Finished(run) => {
            (run.to_string(), false)
        }
    };
    let prefix = if ask { "running-ask" } else { "running" };
    let (row_id, now_id, state_id) = if ask {
        (
            format!("running-ask-{id}"),
            format!("running-ask-now-{id}"),
            format!("running-ask-state-{id}"),
        )
    } else {
        (
            format!("running-run-{id}"),
            format!("running-now-{id}"),
            format!("running-state-{id}"),
        )
    };
    let label = format!(
        "{} · {} · #{} · {}",
        row.agent, row.project, row.channel, row.title
    );
    let place = format!("{} · #{}", row.project, row.channel);
    let actions = render_actions(&row, cx);
    div()
        .id(SharedString::from(row_id))
        .flex()
        .items_center()
        .gap(px(12.))
        .min_h(px(ROW_MIN_HEIGHT))
        .px(px(12.))
        .py(px(8.))
        .border_b_1()
        .border_color(color("border-100"))
        .when(ask, |row| row.bg(color("warning-subtle")))
        .aria_label(label)
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(10.))
                .w(px(AGENT_COL))
                .child(monogram(&row.agent, 24.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(row.agent),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(color("text-tertiary"))
                                .child(place),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().truncate().child(row.title))
                .child(
                    div()
                        .id(SharedString::from(now_id))
                        .truncate()
                        .text_size(px(12.))
                        .text_color(color("text-secondary"))
                        .when(row.now_mono, |el| el.font_family(mono_font()))
                        .aria_label(row.now.clone())
                        .child(row.now)
                        .test_support(),
                ),
        )
        .child(
            div().flex_none().w(px(RUNTIME_COL)).child(
                match &row.runtime {
                    Some(runtime) => runtime_badge_for(runtime, row.chip.clone()),
                    None => chip(row.chip.clone()),
                }
                .id(SharedString::from(format!("{prefix}-chip-{id}")))
                .aria_label(row.chip)
                .test_support(),
            ),
        )
        .child(
            div()
                .id(SharedString::from(state_id))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .w(px(STATUS_COL))
                .overflow_hidden()
                .text_size(px(12.))
                .text_color(color(row.state_token))
                .when(row.state_mono, |el| el.font_family(mono_font()))
                .aria_label(row.state.clone())
                .map(|el| match row.rolling_from {
                    Some(from) => el
                        .child(spinner(
                            SharedString::from(format!("running-spinner-{id}")),
                            12.,
                            row.state_token,
                        ))
                        .child(rolling_text(
                            SharedString::from(format!("running-elapsed-{id}")),
                            &from,
                            &row.state,
                        )),
                    None => el.child(div().min_w_0().truncate().child(row.state)),
                })
                .test_support(),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .justify_end()
                .gap(px(6.))
                .w(px(ACTIONS_COL))
                .children(actions),
        )
        .test_support()
}

/// A live terminal session: project and folder, title, "live" and Open.
fn render_terminal_row(
    session: &TerminalSession,
    project: String,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let id = session.id.clone();
    let title = if session.title.is_empty() {
        "Untitled session".to_string()
    } else {
        session.title.clone()
    };
    let place = format!("{project} · {}", cut_start(&session.cwd, 40));
    let open = id.clone();
    div()
        .id(SharedString::from(format!("running-terminal-{id}")))
        .flex()
        .items_center()
        .gap(px(12.))
        .min_h(px(ROW_MIN_HEIGHT))
        .px(px(12.))
        .py(px(8.))
        .border_b_1()
        .border_color(color("border-100"))
        .aria_label(format!("{project} · {title}"))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(10.))
                .w(px(AGENT_COL))
                .child(monogram("Claude", 24.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(project),
                        )
                        .child(
                            div()
                                .truncate()
                                .font_family(mono_font())
                                .text_size(px(12.))
                                .text_color(color("text-tertiary"))
                                .child(place),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().truncate().child(title))
                .child(
                    div()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(color("text-secondary"))
                        .child("Claude Code in your terminal"),
                ),
        )
        .child(div().flex_none().w(px(RUNTIME_COL)).child(chip("terminal")))
        .child(
            div()
                .id(SharedString::from(format!("running-terminal-state-{id}")))
                .flex_none()
                .w(px(STATUS_COL))
                .text_size(px(12.))
                .text_color(color("accent-blue"))
                .aria_label("live")
                .child("live")
                .test_support(),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .justify_end()
                .w(px(ACTIONS_COL))
                .child(
                    Button::new(SharedString::from(format!("running-terminal-open-{id}")))
                        .ghost()
                        .small()
                        .label("Open")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_terminal(open.clone(), window, cx);
                        })),
                ),
        )
        .test_support()
}

/// Allow once / Deny or Approve / Decline, Open, Watch and Stop, by group.
fn render_actions(row: &Row, cx: &mut Context<Workspace>) -> Vec<AnyElement> {
    let thread = row.thread;
    let open = |id: String, label: &'static str, cx: &mut Context<Workspace>| {
        Button::new(SharedString::from(id))
            .ghost()
            .small()
            .label(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_from_running(thread, window, cx);
            }))
            .into_any_element()
    };
    let stop = |run: RunId, cx: &mut Context<Workspace>| {
        Button::new(SharedString::from(format!("running-stop-{run}")))
            .outline()
            .small()
            .text_color(color("danger"))
            .icon(IconName::Square)
            .label("Stop")
            .on_click(cx.listener(move |this, _, window, cx| this.stop_run(run, window, cx)))
            .into_any_element()
    };
    match row.kind {
        RowKind::Ask {
            id,
            handoff,
            paused,
            blocked,
            retryable,
        } => {
            let mut buttons = Vec::new();
            if !paused {
                let (yes, no) = if handoff {
                    ("Approve", "Decline")
                } else if blocked {
                    ("Allow and retry", "Dismiss")
                } else {
                    ("Allow once", "Deny")
                };
                if retryable {
                    buttons.push(
                        Button::new(SharedString::from(format!("running-allow-{id}")))
                            .outline()
                            .small()
                            .label(yes)
                            .disabled(row.deciding)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.decide(id, true, None, window, cx);
                            }))
                            .into_any_element(),
                    );
                }
                buttons.push(
                    Button::new(SharedString::from(format!("running-deny-{id}")))
                        .ghost()
                        .small()
                        .label(no)
                        .disabled(row.deciding)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.decide(id, false, None, window, cx);
                        }))
                        .into_any_element(),
                );
            }
            buttons.push(open(format!("running-ask-open-{id}"), "Open", cx));
            buttons
        }
        RowKind::Working(run) => vec![
            open(format!("running-watch-{run}"), "Watch", cx),
            stop(run, cx),
        ],
        RowKind::Queued(run) => vec![stop(run, cx)],
        RowKind::Finished(run) => vec![open(format!("running-open-{run}"), "Open", cx)],
    }
}

#[cfg(test)]
mod tests {
    use super::{queued_line, stop_all_look, summary_line};

    #[test]
    fn stop_all_is_red_only_while_it_can_stop_something() {
        assert_eq!(stop_all_look(true, false), (true, "danger"));
        assert_eq!(stop_all_look(false, false), (false, "text-tertiary"));
        assert_eq!(stop_all_look(true, true), (false, "text-tertiary"));
    }

    #[test]
    fn a_queued_run_says_what_it_waits_for() {
        assert_eq!(
            queued_line(Some("Forge")),
            "Starts after Forge finishes in this thread"
        );
        assert_eq!(queued_line(None), "Starts when a run slot is free");
    }

    #[test]
    fn the_summary_counts_agents_projects_and_asks() {
        assert_eq!(summary_line(0, 0, 0), "Nothing is running.");
        assert_eq!(
            summary_line(3, 3, 2),
            "3 agents working across 3 projects. 2 are waiting on you."
        );
        assert_eq!(summary_line(1, 1, 0), "1 agent working across 1 project.");
        assert_eq!(
            summary_line(2, 1, 1),
            "2 agents working across 1 project. 1 is waiting on you."
        );
        assert_eq!(summary_line(0, 0, 1), "1 agent is waiting on you.");
        assert_eq!(summary_line(0, 0, 2), "2 agents are waiting on you.");
    }
}
