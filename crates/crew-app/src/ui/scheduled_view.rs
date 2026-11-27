//! Scheduled: every job, in place of the body beside the sidebar. Jobs
//! that are on come first, then a Paused section. Each row shows what
//! the job does, its agent, how it repeats, its last runs as a strip and
//! when it runs next; hovered, it offers Run now and a menu.

use crew_core::event::{Event, RunStatus};
use crew_core::ids::{JobId, ThreadId};
use crew_core::schedule::describe;
use crew_core::store::{ApprovalKind, Author, NewJob, StoredJob, StoredRun, Tell};
use crew_proto::Request;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::approval_card::input_text;
use super::common::{chip, clock, elapsed, monogram, now_ms, spinner, switch};
use super::job_runs::{Tick, days_after, first_line, local_format, tick, tick_mark};
use super::running_view::local_midnight;
use super::workspace::Workspace;
use crate::model::State;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// The board's clock, before "Scheduled" and on ⌘K's row.
pub const CLOCK_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="8" cy="8" r="6"/><path d="M8 4.5V8l2.5 1.5"/></svg>"#;
/// The same clock drawn thin, large on the empty page.
const CLOCK_THIN_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1" stroke-linecap="round"><circle cx="8" cy="8" r="6"/><path d="M8 4.5V8l2.5 1.5"/></svg>"#;

/// Column widths of a row after the job's: agent, repeats, last runs,
/// next and the switch.
const AGENT_COL: f32 = 168.;
const REPEATS_COL: f32 = 176.;
const LAST_COL: f32 = 200.;
const NEXT_COL: f32 = 132.;
const SWITCH_COL: f32 = 36.;
const COL_GAP: f32 = 16.;
const PAGE_MAX: f32 = 1100.;
/// Runs the strip shows.
const STRIP_RUNS: usize = 20;
/// How many days back a run's day reads as a weekday.
const WEEKDAY_DAYS: i64 = 6;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

const AGENT_DELETED: &str = "Agent deleted";
const EMPTY_TITLE: &str = "No scheduled jobs yet";
const EMPTY_TEXT: &str = "An agent can check something on a schedule and tell you when it changes.";
const AWAKE_NOTE: &str = "Jobs run while your Mac is awake.";

/// What a suggestion on the empty page fills the new job form with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPrefill {
    PageCheck,
    PrMorning,
    WeeklySummary,
}

/// The empty page's suggestions, in the board's order.
const SUGGESTIONS: [(&str, &str, JobPrefill); 3] = [
    ("page", "Check a page every hour", JobPrefill::PageCheck),
    ("prs", "Review my PRs each morning", JobPrefill::PrMorning),
    (
        "weekly",
        "Weekly summary of my work",
        JobPrefill::WeeklySummary,
    ),
];

/// What the Scheduled page shows.
pub(super) struct ScheduledView {
    /// The row under the pointer, which shows its actions.
    hovered: Option<JobId>,
    /// The row whose menu is open, which keeps its actions.
    menu_open: Option<JobId>,
}

/// What the Last runs column shows.
enum Last {
    /// The strip, then the last run: "2 new PRs · 14:30".
    Strip {
        text: String,
        /// The text is green: the run found something.
        found: bool,
        time: Option<String>,
    },
    /// A state worth more than the strip: running, waiting, failed,
    /// missed while asleep, or paused by crewd.
    State {
        tick: Option<Tick>,
        spinning: bool,
        text: String,
        token: &'static str,
        /// The line under it; backticks mark code.
        detail: String,
    },
}

/// The button a hovered row leads with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    RunNow,
    Stop,
    OpenThread,
    Edit,
}

/// One job's row, worked out from the state.
struct JobRow {
    id: JobId,
    thread: ThreadId,
    name: String,
    desc: String,
    new: bool,
    agent: String,
    runtime: &'static str,
    repeats: (String, String),
    strip: Vec<Tick>,
    last: Last,
    next: Option<(String, String)>,
    enabled: bool,
    action: RowAction,
}

/// A past run's time: `14:30` today, `Mon 24` this week, else `21 Nov`.
pub fn run_time(ms: i64, now: i64) -> String {
    let today = local_midnight(now);
    if ms >= today {
        return clock(ms);
    }
    let format = if ms >= today - WEEKDAY_DAYS * DAY_MS {
        "%a %-d"
    } else {
        "%-d %b"
    };
    local_format(ms, format)
}

/// When a job runs next, as two lines: `15:00` today, `Tomorrow` and
/// `14:45`, or `Mon 1 Dec` and `9:00`.
pub fn next_lines(due: i64, now: i64) -> (String, String) {
    let time = local_format(due, "%-H:%M");
    match days_after(due, now) {
        0 => (time, String::new()),
        1 => ("Tomorrow".into(), time),
        _ => (local_format(due, "%a %-d %b"), time),
    }
}

/// "4 jobs on · next run at 15:00", tomorrow's or a later day's.
pub fn summary_line(on: usize, next: Option<i64>, now: i64) -> String {
    let jobs = if on == 1 { "job" } else { "jobs" };
    let mut line = format!("{on} {jobs} on");
    if let Some(due) = next {
        let time = local_format(due, "%-H:%M");
        let when = match days_after(due, now) {
            0 => format!("at {time}"),
            1 => format!("tomorrow at {time}"),
            _ => format!("{} at {time}", local_format(due, "%a %-d %b")),
        };
        line.push_str(&format!(" · next run {when}"));
    }
    line
}

/// "Missed 6 runs while the Mac slept, ran once on wake".
fn missed_line(n: u32) -> String {
    let runs = if n == 1 { "run" } else { "runs" };
    format!("Missed {n} {runs} while the Mac slept, ran once on wake")
}

/// "2 runs skipped".
fn skipped_line(n: u32) -> String {
    let runs = if n == 1 { "run" } else { "runs" };
    format!("{n} {runs} skipped")
}

/// The Mac's time zone, such as `Europe/Madrid`, if it can tell.
fn time_zone() -> Option<String> {
    let link = std::fs::read_link("/etc/localtime").ok()?;
    let path = link.to_string_lossy();
    let (_, zone) = path.split_once("zoneinfo/")?;
    Some(zone.to_string())
}

/// The note under the table.
fn awake_note() -> String {
    match time_zone() {
        Some(zone) => format!("{AWAKE_NOTE} Times are in {zone}."),
        None => AWAKE_NOTE.to_string(),
    }
}

impl Workspace {
    /// Shows Scheduled in place of the body, beside the sidebar.
    pub(super) fn show_scheduled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_pages(cx);
        self.leave_for_terminal(cx);
        self.scheduled = Some(ScheduledView {
            hovered: None,
            menu_open: None,
        });
        self.focus_workspace(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// When you last looked at `thread`, in Unix ms. The thread open now
    /// counts as read.
    pub(super) fn thread_seen(&self, thread: ThreadId) -> i64 {
        if self.nav.thread == Some(thread) && self.scheduled.is_none() {
            return i64::MAX;
        }
        self.nav.thread_viewed.get(&thread).copied().unwrap_or(0)
    }

    /// The Scheduled row's counts: jobs that found something you haven't
    /// read, and jobs waiting on you.
    pub(super) fn scheduled_counts(&self) -> (usize, usize) {
        self.state()
            .scheduled_counts(|thread| self.thread_seen(thread))
    }

    /// Whether the page shows a running job's time, which ticks.
    pub(super) fn scheduled_ticks(&self) -> bool {
        self.scheduled.is_some()
            && self.state().jobs().iter().any(|job| {
                self.state()
                    .job_runs(job.id)
                    .last()
                    .is_some_and(|r| r.status == RunStatus::Working)
            })
    }

    fn set_job_enabled(
        &mut self,
        job: JobId,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request(Request::SetJobEnabled { job, enabled }, window, cx);
    }

    fn duplicate_job(&mut self, job: JobId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(j) = self.state().job(job) else {
            return;
        };
        let copy = NewJob {
            name: format!("{} copy", j.name),
            body: j.body.clone(),
            skill: j.skill.clone(),
            model_override: j.model_override.clone(),
            agent: j.agent,
            channel: j.channel,
            repeat: j.repeat,
            tell: j.tell,
            on_ask: j.on_ask,
            missed: j.missed,
            enabled: j.enabled,
        };
        self.request(Request::CreateJob { job: copy }, window, cx);
    }

    pub(super) fn open_job_thread(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state().thread(thread).is_none() {
            return;
        }
        self.scheduled = None;
        self.open_thread(thread, window, cx);
    }

    fn hover_job(&mut self, job: JobId, hovered: bool, cx: &mut Context<Self>) {
        let Some(view) = self.scheduled.as_mut() else {
            return;
        };
        if hovered {
            view.hovered = Some(job);
        } else if view.hovered == Some(job) {
            view.hovered = None;
        }
        cx.notify();
    }

    fn job_menu_opened(&mut self, job: JobId, open: bool, cx: &mut Context<Self>) {
        let Some(view) = self.scheduled.as_mut() else {
            return;
        };
        if open {
            view.menu_open = Some(job);
        } else if view.menu_open == Some(job) {
            view.menu_open = None;
        }
        cx.notify();
    }

    /// Works out what `job`'s row shows at `now`.
    fn job_row(&self, job: &StoredJob, now: i64) -> JobRow {
        let state = self.state();
        let runs = state.job_runs(job.id);
        let waiting = state.job_waiting(job.id);
        let strip = runs[runs.len().saturating_sub(STRIP_RUNS)..]
            .iter()
            .map(|r| tick(state, job, r))
            .collect();
        let new = !waiting
            && state
                .job_found_at(job.id)
                .is_some_and(|at| at > self.thread_seen(job.thread));
        let (agent, runtime) = state.agent(job.agent).map_or_else(
            || (String::new(), ""),
            |a| {
                let runtime = match a.spec.runtime {
                    crew_core::agent::Runtime::ClaudeCode => "claude",
                    crew_core::agent::Runtime::Codex => "codex",
                };
                (a.spec.name.clone(), runtime)
            },
        );
        let (first, second) = describe(&job.repeat);
        let second = if !second.is_empty() {
            second
        } else if job.tell == Tell::EveryRun {
            "Always tells you".into()
        } else {
            state
                .channel(job.channel)
                .and_then(|c| state.project(c.project))
                .map(|p| p.name.clone())
                .unwrap_or_default()
        };
        let (last, action) = self.last_runs(job, runs.last().copied(), waiting, now);
        JobRow {
            id: job.id,
            thread: job.thread,
            name: job.name.clone(),
            desc: first_line(&job.body),
            new,
            agent,
            runtime,
            repeats: (first, second),
            strip,
            last,
            next: job
                .next_due
                .filter(|_| job.enabled)
                .map(|due| next_lines(due, now)),
            enabled: job.enabled,
            action,
        }
    }

    /// The Last runs column and the button a hovered row leads with.
    fn last_runs(
        &self,
        job: &StoredJob,
        latest: Option<&StoredRun>,
        waiting: bool,
        now: i64,
    ) -> (Last, RowAction) {
        if let Some(reason) = job.paused_reason.as_ref().filter(|_| !job.enabled) {
            let detail = if reason == AGENT_DELETED {
                "Paused until you pick another agent".to_string()
            } else {
                String::new()
            };
            let last = Last::State {
                tick: None,
                spinning: false,
                text: reason.clone(),
                token: "text-secondary",
                detail,
            };
            return (last, RowAction::Edit);
        }
        // Off by your hand: that says more than its last run.
        if !job.enabled {
            let last = Last::Strip {
                text: "Paused by you".into(),
                found: false,
                time: job.paused_at.map(|at| local_format(at, "%-d %b")),
            };
            return (last, RowAction::RunNow);
        }
        let Some(run) = latest else {
            let last = Last::Strip {
                text: "No runs yet".into(),
                found: false,
                time: None,
            };
            return (last, RowAction::RunNow);
        };
        let ended = run.ended_at.unwrap_or(run.started_at);
        match run.status {
            RunStatus::Queued | RunStatus::Working if waiting => {
                let mut parts: Vec<String> = self.wants(run).into_iter().collect();
                if job.skipped > 0 {
                    parts.push(skipped_line(job.skipped));
                }
                let last = Last::State {
                    tick: Some(Tick::Waiting),
                    spinning: false,
                    text: "Waiting on you".into(),
                    token: "warning",
                    detail: parts.join(" · "),
                };
                (last, RowAction::OpenThread)
            }
            RunStatus::Queued | RunStatus::Working => {
                let last = Last::State {
                    tick: None,
                    spinning: true,
                    text: format!("Running · {}", elapsed(now - run.started_at)),
                    token: "accent-blue",
                    detail: format!("Started {}", clock(run.started_at)),
                };
                (last, RowAction::Stop)
            }
            RunStatus::Failed => {
                let last = Last::State {
                    tick: Some(Tick::Failed),
                    spinning: false,
                    text: format!("Failed · {}", clock(ended)),
                    token: "danger",
                    detail: self.failure(run),
                };
                (last, RowAction::RunNow)
            }
            _ => {
                let found = State::job_run_found(job, run);
                let text = match (&run.summary, &run.status) {
                    (Some(summary), _) => summary.to_string(),
                    (None, RunStatus::Stopped) => "Stopped".into(),
                    (None, RunStatus::Interrupted) => "Interrupted".into(),
                    (None, _) if found => "Found something".into(),
                    (None, _) => "Nothing new".into(),
                };
                let last = if run.missed > 0 {
                    Last::State {
                        tick: Some(if found { Tick::Found } else { Tick::Quiet }),
                        spinning: false,
                        text: format!("{text} · {}", clock(ended)),
                        token: if found { "success" } else { "text-secondary" },
                        detail: missed_line(run.missed),
                    }
                } else {
                    Last::Strip {
                        text,
                        found: run.found == Some(true),
                        time: Some(run_time(ended, now)),
                    }
                };
                (last, RowAction::RunNow)
            }
        }
    }

    /// "Wants to run `glab mr list`", from the run's pending ask.
    fn wants(&self, run: &StoredRun) -> Option<String> {
        self.state()
            .pending_approvals(run.thread)
            .into_iter()
            .filter(|a| a.run == run.id)
            .find_map(|a| match &a.kind {
                ApprovalKind::Permission { input, .. } => {
                    let text = input_text(input);
                    let line = text.lines().next().unwrap_or_default().to_string();
                    Some(format!("Wants to run `{line}`"))
                }
                _ => None,
            })
    }

    /// Why the run failed, as its thread says: crewd's note about it, or
    /// the error it logged.
    fn failure(&self, run: &StoredRun) -> String {
        let state = self.state();
        let note = state
            .messages(run.thread)
            .iter()
            .rev()
            .find(|m| m.run == Some(run.id) && m.author == Author::System)
            .map(|m| m.body.clone());
        let logged = || {
            state
                .events(run.thread)
                .iter()
                .rev()
                .find_map(|e| match &e.event {
                    Event::Error { message } if e.run == run.id => Some(message.clone()),
                    _ => None,
                })
        };
        note.or_else(logged)
            .or_else(|| run.summary.as_deref().map(str::to_string))
            .map(|text| first_line(&text))
            .unwrap_or_default()
    }

    /// Scheduled, with the sidebar, in place of the body row.
    pub(super) fn render_scheduled(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(self.render_sidebar(cx))
            .child(self.render_scheduled_page(cx))
            .into_any_element()
    }

    fn render_scheduled_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let now = now_ms();
        let jobs = state.jobs();
        let on: Vec<&StoredJob> = jobs.iter().copied().filter(|j| j.enabled).collect();
        let paused: Vec<&StoredJob> = jobs.iter().copied().filter(|j| !j.enabled).collect();
        let next = on.iter().filter_map(|j| j.next_due).min();
        let hovered = self
            .scheduled
            .as_ref()
            .and_then(|v| v.menu_open.or(v.hovered));
        let row = |job: &StoredJob, cx: &mut Context<Self>| {
            let row = self.job_row(job, now);
            let shown = hovered == Some(row.id);
            render_job(row, shown, cx).into_any_element()
        };
        let on_rows: Vec<AnyElement> = on.iter().map(|j| row(j, cx)).collect();
        let paused_rows: Vec<AnyElement> = paused.iter().map(|j| row(j, cx)).collect();
        let empty = jobs.is_empty();
        div()
            .id("scheduled")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px(ui_px(32.))
            .pt(scale::row(24.))
            .pb(scale::row(32.))
            .child(
                div()
                    .max_w(ui_px(PAGE_MAX))
                    .child(render_page_header(
                        (!empty).then(|| summary_line(on.len(), next, now)),
                        cx,
                    ))
                    .map(|page| {
                        if empty {
                            page.child(render_empty(cx))
                        } else {
                            page.child(render_table(on_rows, paused_rows))
                                .child(render_legend())
                        }
                    }),
            )
            .test_support()
    }
}

/// "Scheduled", the jobs on and the next run, and New job.
fn render_page_header(summary: Option<String>, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .items_end()
        .gap(ui_px(12.))
        .mb(scale::row(20.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .id("scheduled-title")
                        .text_size(scale::text(24.))
                        .line_height(scale::text(32.))
                        .font_weight(weight(FontWeight::SEMIBOLD))
                        .aria_label("Scheduled")
                        .child("Scheduled")
                        .test_support(),
                )
                .children(summary.map(|summary| {
                    div()
                        .id("scheduled-summary")
                        .text_size(scale::text(12.))
                        .text_color(color("text-tertiary"))
                        .aria_label(summary.clone())
                        .child(summary)
                        .test_support()
                })),
        )
        .child(
            Button::new("scheduled-new")
                .primary()
                .icon(IconName::Plus)
                .label("New job")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_job_form(None, None, window, cx);
                })),
        )
}

/// The jobs on, then the paused ones under their own header.
fn render_table(on: Vec<AnyElement>, paused: Vec<AnyElement>) -> impl IntoElement {
    let paused_count = paused.len();
    div()
        .border_1()
        .border_color(color("border-100"))
        .rounded(ui_px(8.))
        .overflow_hidden()
        .child(
            grid_row()
                .py(scale::row(8.))
                .bg(color("surface-raised"))
                .child(th("Job").flex_1().min_w_0().test_support())
                .child(th("Agent").w(ui_px(AGENT_COL)).test_support())
                .child(th("Repeats").w(ui_px(REPEATS_COL)).test_support())
                .child(th("Last runs").w(ui_px(LAST_COL)).test_support())
                .child(th("Next").w(ui_px(NEXT_COL)).test_support())
                .child(div().flex_none().w(ui_px(SWITCH_COL))),
        )
        .children(on)
        .when(paused_count > 0, |table| {
            let text = format!("Paused · {paused_count}");
            table
                .child(
                    grid_row()
                        .id("scheduled-paused")
                        .py(scale::row(8.))
                        .border_t_1()
                        .border_color(color("border-100"))
                        .bg(color("surface-raised"))
                        .aria_label(text.clone())
                        .child(th(&text))
                        .test_support(),
                )
                .children(paused)
        })
}

/// A table row's frame: the columns side by side.
fn grid_row() -> Div {
    div()
        .flex()
        .items_center()
        .gap(ui_px(COL_GAP))
        .px(ui_px(16.))
}

fn th(text: &str) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("scheduled-th-{text}")))
        .flex_none()
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-tertiary"))
        .aria_label(text.to_string())
        .child(text.to_string())
}

/// 12 px grey text, the board's `.sub`.
fn sub() -> Div {
    div()
        .text_size(scale::text(12.))
        .text_color(color("text-tertiary"))
}

/// `text` with its backticked parts in mono, on one line.
fn with_code(text: &str) -> Vec<AnyElement> {
    text.split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(i, part)| {
            let el = div().flex_none().child(part.to_string());
            if i % 2 == 1 {
                el.font_family(code_family())
                    .text_size(scale::mono(12.))
                    .into_any_element()
            } else {
                el.into_any_element()
            }
        })
        .collect()
}

fn render_job(row: JobRow, hovered: bool, cx: &mut Context<Workspace>) -> impl IntoElement {
    let id = row.id;
    let thread = row.thread;
    let enabled = row.enabled;
    grid_row()
        .id(SharedString::from(format!("job-{id}")))
        .py(scale::row(12.))
        .border_t_1()
        .border_color(color("border-100"))
        .cursor_pointer()
        .when(hovered, |r| r.bg(color("surface-100")))
        .when(!enabled, |r| r.text_color(color("text-secondary")))
        .aria_label(row.name.clone())
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            this.hover_job(id, *hovered, cx);
        }))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_job_thread(thread, window, cx);
        }))
        .child(render_job_name(&row))
        .child(render_job_agent(&row))
        .child(render_repeats(&row))
        .child(render_last(&row))
        .child(if hovered {
            render_row_actions(&row, cx).into_any_element()
        } else {
            render_next(&row).into_any_element()
        })
        .child(
            div()
                .id(SharedString::from(format!("job-toggle-{id}")))
                .flex()
                .flex_none()
                .w(ui_px(SWITCH_COL))
                .cursor_pointer()
                .role(Role::Switch)
                .aria_label(if enabled { "On" } else { "Off" })
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.set_job_enabled(id, !enabled, window, cx);
                }))
                .child(switch(enabled))
                .test_support(),
        )
        .test_support()
}

fn render_job_name(row: &JobRow) -> impl IntoElement {
    let id = row.id;
    div()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap(ui_px(8.))
                .font_weight(weight(FontWeight::MEDIUM))
                .child(
                    div()
                        .id(SharedString::from(format!("job-name-{id}")))
                        .min_w_0()
                        .truncate()
                        .aria_label(row.name.clone())
                        .child(row.name.clone())
                        .test_support(),
                )
                .when(row.new, |name| {
                    name.child(
                        count_pill("New", "success-subtle", "success")
                            .id(SharedString::from(format!("job-new-{id}")))
                            .aria_label("New")
                            .test_support(),
                    )
                }),
        )
        .child(
            sub()
                .id(SharedString::from(format!("job-desc-{id}")))
                .mt(ui_px(2.))
                .truncate()
                .aria_label(row.desc.clone())
                .child(row.desc.clone())
                .test_support(),
        )
}

/// The board's `.cnt`: a small round count, here in `bg` and `token`.
pub(super) fn count_pill(text: impl Into<SharedString>, bg: &str, token: &str) -> Div {
    div()
        .flex_none()
        .min_w(ui_px(16.))
        .px(ui_px(5.))
        .rounded_full()
        .bg(color(bg))
        .text_color(color(token))
        .text_size(scale::text(11.))
        .line_height(scale::text(16.))
        .font_weight(weight(FontWeight::NORMAL))
        .text_center()
        .child(text.into())
}

fn render_job_agent(row: &JobRow) -> impl IntoElement {
    let label = format!("{} · {}", row.agent, row.runtime);
    div()
        .id(SharedString::from(format!("job-agent-{}", row.id)))
        .flex()
        .flex_none()
        .items_center()
        .gap(ui_px(8.))
        .w(ui_px(AGENT_COL))
        .min_w_0()
        .when(!row.enabled, |a| a.opacity(0.6))
        .aria_label(label)
        .child(monogram(&row.agent, 20.))
        .child(div().min_w_0().truncate().child(row.agent.clone()))
        .child(chip(row.runtime))
        .test_support()
}

fn render_repeats(row: &JobRow) -> impl IntoElement {
    let (first, second) = row.repeats.clone();
    div()
        .id(SharedString::from(format!("job-repeats-{}", row.id)))
        .flex_none()
        .w(ui_px(REPEATS_COL))
        .min_w_0()
        .aria_label(format!("{first} · {second}"))
        .child(div().truncate().child(first))
        .child(sub().truncate().child(second))
        .test_support()
}

fn render_last(row: &JobRow) -> impl IntoElement {
    let id = row.id;
    let column = div().flex_none().w(ui_px(LAST_COL)).min_w_0();
    match &row.last {
        Last::Strip { text, found, time } => {
            let label = match time {
                Some(time) => format!("{text} · {time}"),
                None => text.clone(),
            };
            let words: Vec<&str> = row.strip.iter().map(|t| t.word()).collect();
            column
                .child(
                    div()
                        .id(SharedString::from(format!("job-strip-{id}")))
                        .flex()
                        .items_center()
                        .gap(ui_px(2.))
                        .when(!row.enabled, |s| s.opacity(0.6))
                        .aria_label(words.join(" "))
                        .children(row.strip.iter().map(|t| tick_mark(*t)))
                        .test_support(),
                )
                .child(
                    sub()
                        .id(SharedString::from(format!("job-last-{id}")))
                        .flex()
                        .mt(ui_px(4.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .aria_label(label)
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .when(*found, |t| t.text_color(color("success")))
                                .child(text.clone()),
                        )
                        .children(
                            time.clone()
                                .map(|time| div().flex_none().child(format!(" · {time}"))),
                        )
                        .test_support(),
                )
        }
        Last::State {
            tick,
            spinning,
            text,
            token,
            detail,
        } => column
            .child(
                div()
                    .id(SharedString::from(format!("job-state-{id}")))
                    .flex()
                    .items_center()
                    .gap(ui_px(6.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_color(color(token))
                    .aria_label(text.clone())
                    .when(*spinning, |line| {
                        line.child(spinner(
                            SharedString::from(format!("job-spinner-{id}")),
                            12.,
                            token,
                        ))
                    })
                    .children(tick.map(tick_mark))
                    .map(|line| match text.split_once(" · ").filter(|_| *spinning) {
                        // The running time reads in mono, as on the board.
                        Some((word, time)) => line.child(format!("{word} · ")).child(
                            div()
                                .font_family(code_family())
                                .text_size(scale::mono(12.))
                                .child(time.to_string()),
                        ),
                        None => line.child(text.clone()),
                    })
                    .test_support(),
            )
            .when(!detail.is_empty(), |column| {
                column.child(
                    sub()
                        .id(SharedString::from(format!("job-detail-{id}")))
                        .flex()
                        .mt(ui_px(2.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .aria_label(detail.replace('`', ""))
                        .children(with_code(detail))
                        .test_support(),
                )
            }),
    }
}

fn render_next(row: &JobRow) -> impl IntoElement {
    let column = div()
        .id(SharedString::from(format!("job-next-{}", row.id)))
        .flex_none()
        .w(ui_px(NEXT_COL));
    match row.next.clone() {
        Some((first, second)) => {
            let label = if second.is_empty() {
                first.clone()
            } else {
                format!("{first} {second}")
            };
            column
                .aria_label(label)
                .child(first)
                .when(!second.is_empty(), |c| c.child(sub().child(second)))
        }
        None => column.aria_label("—").child(sub().child("—")),
    }
    .test_support()
}

/// The hovered row's lead button and its menu, in place of Next.
fn render_row_actions(row: &JobRow, cx: &mut Context<Workspace>) -> impl IntoElement {
    let id = row.id;
    let thread = row.thread;
    let enabled = row.enabled;
    let (key, label) = match row.action {
        RowAction::RunNow => ("run-now", "Run now"),
        RowAction::Stop => ("stop", "Stop"),
        RowAction::OpenThread => ("open", "Open thread"),
        RowAction::Edit => ("edit", "Edit job"),
    };
    let action = row.action;
    let this = cx.entity().downgrade();
    let opened = this.clone();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(ui_px(2.))
        .w(ui_px(NEXT_COL))
        .ml(ui_px(-8.))
        .child(
            Button::new(SharedString::from(format!("job-{key}-{id}")))
                .outline()
                .small()
                .label(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    match action {
                        RowAction::RunNow => {
                            this.request(Request::RunJobNow { job: id }, window, cx);
                        }
                        RowAction::Stop => this.stop_thread(thread, window, cx),
                        RowAction::OpenThread => this.open_job_thread(thread, window, cx),
                        RowAction::Edit => this.open_job_form(Some(id), None, window, cx),
                    }
                })),
        )
        .child(
            Button::new(SharedString::from(format!("job-more-{id}")))
                .ghost()
                .small()
                .icon(IconName::Ellipsis)
                .accessibility_label("More")
                .on_click(|_, _, cx| cx.stop_propagation())
                .dropdown_menu(move |popup, _, _| job_menu(popup, this.clone(), id, enabled))
                .on_open_change(move |open, _, cx| {
                    opened
                        .update(cx, |this, cx| this.job_menu_opened(id, *open, cx))
                        .ok();
                }),
        )
}

/// Edit, Duplicate, Pause and Delete.
fn job_menu(menu: PopupMenu, this: WeakEntity<Workspace>, job: JobId, enabled: bool) -> PopupMenu {
    let act = |f: fn(&mut Workspace, JobId, &mut Window, &mut Context<Workspace>)| {
        let this = this.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            this.update(cx, |this, cx| f(this, job, window, cx)).ok();
        }
    };
    let mut menu = menu
        .item(
            PopupMenuItem::new("Edit").on_click(act(|this, job, window, cx| {
                this.open_job_form(Some(job), None, window, cx)
            })),
        )
        .item(
            PopupMenuItem::new("Duplicate").on_click(act(|this, job, window, cx| {
                this.duplicate_job(job, window, cx)
            })),
        );
    if enabled {
        menu = menu.item(
            PopupMenuItem::new("Pause").on_click(act(|this, job, window, cx| {
                this.set_job_enabled(job, false, window, cx)
            })),
        );
    }
    menu.separator().item(
        PopupMenuItem::element(|_, _| {
            div()
                .id("job-menu-delete")
                .text_color(color("danger"))
                .aria_label("Delete")
                .child("Delete")
                .test_support()
        })
        .on_click(act(|this, job, window, cx| {
            this.request(Request::DeleteJob { job }, window, cx)
        })),
    )
}

/// The strip's colours, and the note about sleep and time zone.
fn render_legend() -> impl IntoElement {
    let item = |tick: Tick, text: &'static str| {
        div()
            .flex()
            .items_center()
            .gap(ui_px(6.))
            .child(tick_mark(tick))
            .child(text)
            .id(SharedString::from(format!(
                "scheduled-legend-{}",
                tick.word()
            )))
            .aria_label(text)
            .test_support()
    };
    let note = awake_note();
    sub()
        .flex()
        .items_center()
        .gap(ui_px(16.))
        .mt(scale::row(12.))
        .child(item(Tick::Quiet, "Nothing new"))
        .child(item(Tick::Found, "Found something"))
        .child(item(Tick::Waiting, "Waiting on you"))
        .child(item(Tick::Failed, "Failed"))
        .child(div().flex_1())
        .child(
            div()
                .id("scheduled-note")
                .aria_label(note.clone())
                .child(note)
                .test_support(),
        )
}

/// No jobs yet: what a job does, three suggestions and New job.
fn render_empty(cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .id("scheduled-empty")
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(scale::row(10.))
        .h(scale::row(300.))
        .border_1()
        .border_color(color("border-100"))
        .rounded(ui_px(8.))
        .bg(color("bg-100"))
        .child(
            Icon::default()
                .data(CLOCK_THIN_SVG)
                .size(ui_px(28.))
                .text_color(color("text-tertiary")),
        )
        .child(
            div()
                .text_size(scale::text(15.))
                .font_weight(weight(FontWeight::SEMIBOLD))
                .id("scheduled-empty-title")
                .aria_label(EMPTY_TITLE)
                .child(EMPTY_TITLE)
                .test_support(),
        )
        .child(
            sub()
                .max_w(ui_px(420.))
                .line_height(scale::text(18.))
                .text_center()
                .id("scheduled-empty-text")
                .aria_label(EMPTY_TEXT)
                .child(EMPTY_TEXT)
                .test_support(),
        )
        .child(
            div()
                .flex()
                .gap(ui_px(8.))
                .mt(scale::row(6.))
                .children(SUGGESTIONS.map(|(key, text, prefill)| {
                    div()
                        .id(SharedString::from(format!("scheduled-suggest-{key}")))
                        .flex()
                        .items_center()
                        .h(scale::row(28.))
                        .px(ui_px(10.))
                        .border_1()
                        .border_color(color("border-200"))
                        .rounded_full()
                        .cursor_pointer()
                        .text_size(scale::text(12.))
                        .text_color(color("text-secondary"))
                        .hover(|s| s.bg(color("surface-200")))
                        .aria_label(text)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_job_form(None, Some(prefill), window, cx);
                        }))
                        .child(text)
                        .test_support()
                })),
        )
        .child(
            div().mt(scale::row(8.)).child(
                Button::new("scheduled-empty-new")
                    .primary()
                    .icon(IconName::Plus)
                    .label("New job")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_job_form(None, None, window, cx);
                    })),
            ),
        )
        .test_support()
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone as _};

    use crate::ui::scheduled_view::{
        missed_line, next_lines, run_time, skipped_line, summary_line,
    };

    fn at(d: u32, h: u32, m: u32) -> i64 {
        Local
            .with_ymd_and_hms(2026, 11, d, h, m, 0)
            .earliest()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn next_reads_as_today_tomorrow_or_a_date() {
        let now = at(27, 14, 50);
        assert_eq!(
            next_lines(at(27, 15, 0), now),
            ("15:00".into(), String::new())
        );
        assert_eq!(
            next_lines(at(28, 14, 45), now),
            ("Tomorrow".into(), "14:45".into())
        );
        assert_eq!(
            next_lines(
                Local
                    .with_ymd_and_hms(2026, 12, 1, 9, 0, 0)
                    .unwrap()
                    .timestamp_millis(),
                now
            ),
            ("Tue 1 Dec".into(), "9:00".into())
        );
    }

    #[test]
    fn past_runs_read_as_a_time_a_weekday_or_a_date() {
        let now = at(27, 15, 0);
        assert_eq!(run_time(at(27, 14, 30), now), "14:30");
        assert_eq!(run_time(at(27, 9, 0), now), "09:00");
        assert_eq!(run_time(at(23, 9, 0), now), "Mon 23");
        assert_eq!(run_time(at(13, 17, 0), now), "13 Nov");
    }

    #[test]
    fn the_summary_counts_jobs_on_and_the_next_run() {
        let now = at(27, 14, 50);
        assert_eq!(
            summary_line(4, Some(at(27, 15, 0)), now),
            "4 jobs on · next run at 15:00"
        );
        assert_eq!(
            summary_line(1, Some(at(28, 9, 0)), now),
            "1 job on · next run tomorrow at 9:00"
        );
        assert_eq!(summary_line(0, None, now), "0 jobs on");
    }

    #[test]
    fn missed_and_skipped_count_runs() {
        assert_eq!(
            missed_line(6),
            "Missed 6 runs while the Mac slept, ran once on wake"
        );
        assert_eq!(
            missed_line(1),
            "Missed 1 run while the Mac slept, ran once on wake"
        );
        assert_eq!(skipped_line(2), "2 runs skipped");
        assert_eq!(skipped_line(1), "1 run skipped");
    }
}
