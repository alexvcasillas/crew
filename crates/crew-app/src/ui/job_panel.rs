//! The right panel of a job's thread, as `V11ScheduleThread` draws it:
//! the job's switch, its settings and next three runs, its last 24 runs
//! as a strip, its recent runs, and Edit job, Pause and Delete job.

use chrono::{Local, TimeZone as _};
use crew_core::event::RunStatus;
use crew_core::ids::JobId;
use crew_core::schedule::{describe, next_after};
use crew_core::store::{Missed, StoredJob, StoredRun, Tell};
use crew_proto::Request;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{chip, elapsed, monogram, now_ms, runtime_chip, switch};
use super::job_runs::{Tick, local_format, same_day, tick, tick_mark};
use super::scheduled_view::{CLOCK_SVG, run_time};
use super::workspace::Workspace;
use crate::model::State;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// The board's panel width.
const PANEL_WIDTH: f32 = 360.;
/// Runs the strip shows.
const STRIP_RUNS: usize = 24;
/// Runs the Recent runs list shows.
const RECENT_RUNS: usize = 5;
/// Next runs listed.
const NEXT_RUNS: usize = 3;
/// The settings' label column.
const LABEL_COL: f32 = 120.;

/// "Every hour, any time".
fn repeats(job: &StoredJob) -> String {
    let (first, second) = describe(&job.repeat);
    if second.is_empty() {
        return first;
    }
    let mut chars = second.chars();
    let second = match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    };
    format!("{first}, {second}")
}

fn tells(tell: Tell) -> &'static str {
    match tell {
        Tell::Changes => "When something changes",
        Tell::EveryRun => "After every run",
    }
}

fn missed(missed: Missed) -> &'static str {
    match missed {
        Missed::OnceOnWake => "Runs once when the Mac wakes",
        Missed::Skip => "Skips them",
    }
}

/// "16:00 · 17:00 · 18:00": the next runs, a time naming its day when
/// the day isn't today's or the one before it.
fn next_runs(job: &StoredJob, now: i64) -> String {
    let Some(due) = job.next_due.filter(|_| job.enabled) else {
        return "Paused".into();
    };
    let mut times = vec![due];
    while times.len() < NEXT_RUNS {
        let last = times[times.len() - 1];
        let Some(at) = Local.timestamp_millis_opt(last).single() else {
            break;
        };
        times.push(next_after(&job.repeat, at).timestamp_millis());
    }
    let mut day = now;
    times
        .into_iter()
        .map(|at| {
            let time = local_format(at, "%H:%M");
            let named = if same_day(at, day) {
                time
            } else {
                format!("{} {time}", super::job_runs::day_word(at, now))
            };
            day = at;
            named
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A recent run's line: its time, what it found and how long it took.
fn recent_run(
    state: &State,
    job: &StoredJob,
    run: &StoredRun,
    now: i64,
) -> (Tick, String, String, String) {
    let t = tick(state, job, run);
    let mut text = match (t, &run.status) {
        (Tick::Running, _) => "Running".to_string(),
        (Tick::Waiting, _) => "Waiting on you".to_string(),
        (Tick::Failed, _) => "Failed".to_string(),
        (_, RunStatus::Stopped) => "Stopped".to_string(),
        (_, RunStatus::Interrupted) => "Interrupted".to_string(),
        (Tick::Found, _) => "Found something".to_string(),
        _ => "Nothing new".to_string(),
    };
    if run.missed > 0 {
        text.push_str(&format!(" · ran on wake, missed {}", run.missed));
    }
    let took = run
        .ended_at
        .map(|end| elapsed(end - run.started_at))
        .unwrap_or_default();
    (t, run_time(run.started_at, now), text, took)
}

impl Workspace {
    /// The job panel, in place of the inspector beside a job's thread.
    pub(super) fn render_job_panel(&self, job: JobId, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state();
        let Some(job) = state.job(job) else {
            return div().into_any_element();
        };
        let now = now_ms();
        let id = job.id;
        let enabled = job.enabled;
        let runs = state.job_runs(id);
        let strip: Vec<Tick> = runs[runs.len().saturating_sub(STRIP_RUNS)..]
            .iter()
            .map(|r| tick(state, job, r))
            .collect();
        let recent: Vec<_> = runs
            .iter()
            .rev()
            .take(RECENT_RUNS)
            .map(|r| recent_run(state, job, r, now))
            .collect();
        let (agent, chip_text) = state.agent(job.agent).map_or_else(
            || ("Agent deleted".to_string(), None),
            |a| (a.spec.name.clone(), Some(runtime_chip(&a.spec))),
        );
        let agent_label = match &chip_text {
            Some(c) => format!("{agent} · {c}"),
            None => agent.clone(),
        };
        let switch_label = if enabled { "Job on" } else { "Job off" };

        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(8.))
            .h(scale::row(44.))
            .pl(ui_px(16.))
            .pr(ui_px(12.))
            .border_b_1()
            .border_color(color("border-100"))
            .child(
                Icon::default()
                    .data(CLOCK_SVG)
                    .size(ui_px(14.))
                    .text_color(color("text-secondary")),
            )
            .child(
                div()
                    .id("job-panel-title")
                    .flex_1()
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .aria_label("Scheduled job")
                    .child("Scheduled job")
                    .test_support(),
            )
            .child(
                div()
                    .id("job-panel-switch")
                    .cursor_pointer()
                    .role(Role::Switch)
                    .aria_label(switch_label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request(
                            Request::SetJobEnabled {
                                job: id,
                                enabled: !enabled,
                            },
                            window,
                            cx,
                        );
                    }))
                    .child(switch(enabled))
                    .test_support(),
            );

        let body = div()
            .id("job-panel-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pt(scale::row(8.))
            .pb(scale::row(16.))
            .px(ui_px(16.))
            .child(text_kv("repeats", "Repeats", false, repeats(job)))
            .child(kv(
                "agent",
                "Agent",
                true,
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(6.))
                    .min_w_0()
                    .child(monogram(&agent, 18.))
                    .child(div().min_w_0().truncate().child(agent.clone()))
                    .children(chip_text.map(chip))
                    .into_any_element(),
                agent_label,
            ))
            .child(text_kv("tell", "Tells you", true, tells(job.tell).into()))
            .child(text_kv(
                "missed",
                "Missed runs",
                true,
                missed(job.missed).into(),
            ))
            .child({
                let next = next_runs(job, now);
                kv(
                    "next",
                    "Next",
                    true,
                    div()
                        .font_family(code_family())
                        .text_size(scale::mono(12.))
                        .child(next.clone())
                        .into_any_element(),
                    next,
                )
            })
            .child(section_label("job-panel-strip-title", "Last 24 runs", 8.))
            .child(div().flex().items_center().gap(ui_px(3.)).children(
                strip.into_iter().enumerate().map(|(i, t)| {
                    div()
                        .id(SharedString::from(format!("job-panel-tick-{i}")))
                        .flex_none()
                        .w(ui_px(8.))
                        .h(ui_px(20.))
                        .rounded(ui_px(1.))
                        .bg(color(t.token()))
                        .aria_label(t.word())
                        .test_support()
                }),
            ))
            .child(section_label("job-panel-recent-title", "Recent runs", 4.))
            .children(
                recent
                    .into_iter()
                    .enumerate()
                    .map(|(i, (t, time, text, took))| {
                        let label = [time.clone(), text.clone(), took.clone()]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" · ");
                        div()
                            .id(SharedString::from(format!("job-panel-run-{i}")))
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .h(scale::row(28.))
                            .text_size(scale::text(12.))
                            .text_color(color("text-secondary"))
                            .whitespace_nowrap()
                            .aria_label(label)
                            .child(tick_mark(t))
                            .child(mono(time).w(ui_px(44.)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(t == Tick::Found, |d| d.text_color(color("success")))
                                    .child(text),
                            )
                            .child(mono(took))
                            .test_support()
                    }),
            );

        let footer = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(8.))
            .px(ui_px(16.))
            .py(scale::row(12.))
            .border_t_1()
            .border_color(color("border-100"))
            .child(
                Button::new("job-panel-edit")
                    .outline()
                    .small()
                    .label("Edit job")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_job_form(Some(id), None, window, cx);
                    })),
            )
            .child(
                Button::new("job-panel-pause")
                    .outline()
                    .small()
                    .label(if enabled { "Pause" } else { "Resume" })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request(
                            Request::SetJobEnabled {
                                job: id,
                                enabled: !enabled,
                            },
                            window,
                            cx,
                        );
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("job-panel-delete")
                    .ghost()
                    .small()
                    .text_color(color("danger"))
                    .label("Delete job")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request(Request::DeleteJob { job: id }, window, cx);
                    })),
            );

        div()
            .id("job-panel")
            .flex()
            .flex_col()
            .flex_none()
            .w(scale::chrome(PANEL_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(color("border-100"))
            .aria_label("Scheduled job")
            .child(header)
            .child(body)
            .child(footer)
            .test_support()
            .into_any_element()
    }
}

/// A setting: its name in grey on the left, its value. All but the first
/// have a hairline above. The value is labelled `job-panel-{key}`.
fn kv(
    key: &str,
    name: &'static str,
    rule: bool,
    value: AnyElement,
    label: String,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("job-panel-row-{key}")))
        .flex()
        .gap(ui_px(8.))
        .py(scale::row(8.))
        .when(rule, |row| {
            row.border_t_1().border_color(color("surface-200"))
        })
        .text_size(scale::text(12.))
        .line_height(scale::line(18.))
        .child(
            div()
                .flex_none()
                .w(ui_px(LABEL_COL))
                .text_color(color("text-tertiary"))
                .child(name),
        )
        .child(
            div()
                .id(SharedString::from(format!("job-panel-{key}")))
                .flex_1()
                .min_w_0()
                .text_color(color("text-primary"))
                .aria_label(label)
                .child(value)
                .test_support(),
        )
}

/// A setting whose value is plain text.
fn text_kv(key: &str, name: &'static str, rule: bool, text: String) -> Stateful<Div> {
    kv(key, name, rule, text.clone().into_any_element(), text)
}

/// A section's title, `below` px over its content.
fn section_label(id: &'static str, text: &'static str, below: f32) -> impl IntoElement {
    div()
        .id(id)
        .mt(scale::row(16.))
        .mb(scale::row(below))
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-secondary"))
        .aria_label(text)
        .child(text)
        .test_support()
}

fn mono(text: String) -> Div {
    div()
        .flex_none()
        .font_family(code_family())
        .text_size(scale::mono(12.))
        .child(text)
}
