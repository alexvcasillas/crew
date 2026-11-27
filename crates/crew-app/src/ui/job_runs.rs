//! What a job's runs look like, shared by the Scheduled page and a job's
//! thread: the strip's ticks and the local times runs are told in.

use chrono::{Local, TimeZone as _};
use crew_core::event::RunStatus;
use crew_core::store::{ApprovalKind, StoredJob, StoredRun};
use gpui_kit::*;

use crate::model::State;
use crate::theme::color;
use crate::theme::scale::ui_px;

/// How many days back a time's day reads as a weekday.
const WEEKDAY_DAYS: i64 = 6;

/// One run in the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tick {
    Quiet,
    Found,
    Waiting,
    Failed,
    Running,
}

impl Tick {
    pub(super) fn token(self) -> &'static str {
        match self {
            Tick::Quiet => "border-200",
            Tick::Found => "success",
            Tick::Waiting => "warning",
            Tick::Failed => "danger",
            Tick::Running => "accent-blue",
        }
    }

    pub(super) fn word(self) -> &'static str {
        match self {
            Tick::Quiet => "quiet",
            Tick::Found => "found",
            Tick::Waiting => "waiting",
            Tick::Failed => "failed",
            Tick::Running => "running",
        }
    }
}

/// The strip's tick for `run` of `job`.
pub(super) fn tick(state: &State, job: &StoredJob, run: &StoredRun) -> Tick {
    match run.status {
        RunStatus::Queued | RunStatus::Working => {
            let waiting = state
                .pending_approvals(run.thread)
                .iter()
                .any(|a| a.run == run.id && !matches!(a.kind, ApprovalKind::Blocked { .. }));
            if waiting {
                Tick::Waiting
            } else {
                Tick::Running
            }
        }
        RunStatus::Failed => Tick::Failed,
        _ if State::job_run_found(job, run) => Tick::Found,
        _ => Tick::Quiet,
    }
}

/// What an ended job run comes to, in words, and whether they read green.
/// A job that tells after every run marks each done run found, so its
/// tick is green, but only a run that said it found something says so.
pub(super) fn ended_words(job: &StoredJob, run: &StoredRun) -> (&'static str, bool) {
    match run.status {
        RunStatus::Stopped => ("Stopped", false),
        RunStatus::Interrupted => ("Interrupted", false),
        _ if run.found == Some(true) => ("Found something", true),
        _ if State::job_run_found(job, run) => ("Done", false),
        _ => ("Nothing new", false),
    }
}

/// One run in the strip, 3 by 12.
pub(super) fn tick_mark(tick: Tick) -> Div {
    div()
        .flex_none()
        .w(ui_px(3.))
        .h(ui_px(12.))
        .rounded(ui_px(1.))
        .bg(color(tick.token()))
}

/// How many local days `ms` is after the day holding `now`.
pub(super) fn days_after(ms: i64, now: i64) -> i64 {
    let day = |t: i64| {
        Local
            .timestamp_millis_opt(t)
            .single()
            .map(|t| t.date_naive())
    };
    match (day(ms), day(now)) {
        (Some(a), Some(b)) => (a - b).num_days(),
        _ => 0,
    }
}

pub(super) fn local_format(ms: i64, format: &str) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or_else(String::new, |t| t.format(format).to_string())
}

/// The first line of `body` with text on it.
pub(super) fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// A time named with its day, as the job form's Next line does:
/// `today 13:00`, `yesterday 20:00`, `tomorrow 09:00`, `Mon 14:00` this
/// week, else `21 Nov 14:00`.
pub(super) fn day_time(ms: i64, now: i64) -> String {
    format!("{} {}", day_word(ms, now), local_format(ms, "%H:%M"))
}

/// The day of `ms` as [`day_time`] names it.
pub(super) fn day_word(ms: i64, now: i64) -> String {
    match days_after(ms, now) {
        0 => "today".into(),
        -1 => "yesterday".into(),
        1 => "tomorrow".into(),
        n if n.abs() <= WEEKDAY_DAYS => local_format(ms, "%a"),
        _ => local_format(ms, "%-d %b"),
    }
}

/// Whether `ms` falls on the same local day as `other`.
pub(super) fn same_day(ms: i64, other: i64) -> bool {
    days_after(ms, other) == 0
}
