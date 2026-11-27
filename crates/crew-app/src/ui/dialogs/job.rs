//! New and edit scheduled job: what to do, who and where, how it
//! repeats, when to tell you, what to do about asks and missed runs.

use chrono::{DateTime, Local, NaiveTime, TimeZone, Weekday};
use crew_core::agent::Runtime;
use crew_core::ids::{AgentId, ChannelId, JobId};
use crew_core::schedule::{Repeat, WeekdaySet, next_after};
use crew_core::store::{JobPatch, Missed, NewJob, OnAsk, StoredJob, Tell};
use crew_proto::Request;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{IconName, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::common::{chip, monogram, runtime_chip, switch};
use super::super::runs_with::skill_chip;
use super::super::scheduled_view::JobPrefill;
use super::super::workspace::Workspace;
use super::DialogKind;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// The dialog's width, as the board has it.
pub(super) const WIDTH: f32 = 680.;
/// How far down the window the dialog sits.
pub(super) const TOP: f32 = 56.;
const LABEL_COL: f32 = 150.;

const NEW_TITLE: &str = "New scheduled job";
const EDIT_TITLE: &str = "Edit scheduled job";
const NO_NAME: &str = "Give the job a name.";
const NO_MESSAGE: &str = "Say what the job should do.";
const NO_AGENT: &str = "Pick an agent.";
const NO_CHANNEL: &str = "Pick a channel.";
const NO_DAYS: &str = "Pick at least one day.";
const BAD_HOURS: &str = "Use a time like 09:00 for the hours.";
const BAD_AT: &str = "Use a time like 09:00.";

/// The intervals the menu offers, in minutes.
const INTERVALS: [u16; 5] = [15, 30, 60, 120, 360];
const WEEK: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];
const WEEKDAYS: [bool; 7] = [true, true, true, true, true, false, false];
/// How many times the Next line shows.
const NEXT_COUNT: usize = 3;
/// Up to this many days ahead, a time's day reads as a weekday.
const WEEKDAY_DAYS: i64 = 6;

/// How a job repeats, as the segmented control picks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepeatKind {
    Every,
    Daily,
    Weekly,
}

/// What the job form holds.
pub(crate) struct JobForm {
    /// The job being edited, or `None` for a new one.
    editing: Option<StoredJob>,
    pub(crate) name: Entity<InputState>,
    /// What to do: the message, after a `/skill` if it runs one.
    pub(crate) what: Entity<TextareaState>,
    agent: Option<AgentId>,
    channel: Option<ChannelId>,
    kind: RepeatKind,
    every: u16,
    only_on: bool,
    every_days: [bool; 7],
    from: Entity<InputState>,
    to: Entity<InputState>,
    /// The time of a daily or weekly job.
    at: Entity<InputState>,
    weekly_days: [bool; 7],
    tell: Tell,
    on_ask: OnAsk,
    missed: Missed,
}

/// What a form starts from.
struct Start {
    name: String,
    body: String,
    skill: Option<String>,
    agent: Option<AgentId>,
    channel: Option<ChannelId>,
    repeat: Repeat,
    tell: Tell,
    on_ask: OnAsk,
    missed: Missed,
}

impl Start {
    fn blank(agent: Option<AgentId>, channel: Option<ChannelId>) -> Self {
        Self {
            name: String::new(),
            body: String::new(),
            skill: None,
            agent,
            channel,
            repeat: Repeat::Every {
                minutes: 60,
                days: None,
                window: None,
            },
            tell: Tell::Changes,
            on_ask: OnAsk::Wait,
            missed: Missed::OnceOnWake,
        }
    }

    fn prefilled(prefill: JobPrefill, agent: Option<AgentId>, channel: Option<ChannelId>) -> Self {
        let nine = time(9, 0);
        let (name, repeat, tell, body) = match prefill {
            JobPrefill::PageCheck => (
                "Page check",
                Repeat::Every {
                    minutes: 60,
                    days: None,
                    window: None,
                },
                Tell::Changes,
                "Check <page> and tell me only when it changes.",
            ),
            JobPrefill::PrMorning => (
                "PRs waiting for my review",
                Repeat::Weekly {
                    days: days_set(&WEEKDAYS),
                    at: nine,
                },
                Tell::Changes,
                "Check GitHub and GitLab for PRs assigned to me that I haven't reviewed yet. List the new ones.",
            ),
            JobPrefill::WeeklySummary => (
                "Weekly work summary",
                Repeat::Weekly {
                    days: WeekdaySet::from_days(&[Weekday::Mon]),
                    at: nine,
                },
                Tell::EveryRun,
                "Summarize what I merged, reviewed and opened on GitHub and GitLab last week, grouped by repo.",
            ),
        };
        Self {
            name: name.into(),
            body: body.into(),
            repeat,
            tell,
            ..Self::blank(agent, channel)
        }
    }

    fn of(job: &StoredJob) -> Self {
        Self {
            name: job.name.clone(),
            body: job.body.clone(),
            skill: job.skill.clone(),
            agent: Some(job.agent),
            channel: Some(job.channel),
            repeat: job.repeat,
            tell: job.tell,
            on_ask: job.on_ask,
            missed: job.missed,
        }
    }
}

fn time(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).expect("a valid time")
}

fn days_set(days: &[bool; 7]) -> WeekdaySet {
    let picked: Vec<Weekday> = WEEK
        .iter()
        .zip(days)
        .filter(|(_, on)| **on)
        .map(|(d, _)| *d)
        .collect();
    WeekdaySet::from_days(&picked)
}

fn days_of(set: WeekdaySet) -> [bool; 7] {
    WEEK.map(|d| set.contains(d))
}

/// `09:00`.
fn time_text(t: NaiveTime) -> String {
    t.format("%H:%M").to_string()
}

fn parse_time(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text.trim(), "%H:%M").ok()
}

/// The interval menu's words: `15 min`, `hour`, `2 hours`.
fn interval_label(minutes: u16) -> String {
    match minutes {
        60 => "hour".into(),
        m if m > 60 && m % 60 == 0 => format!("{} hours", m / 60),
        m => format!("{m} min"),
    }
}

/// `now`, if it isn't `was`: a patch's field.
fn changed<T: PartialEq + Clone>(now: &T, was: &T) -> Option<T> {
    (now != was).then(|| now.clone())
}

/// What to do, split into the skill it runs, from a leading `/skill`,
/// and the message.
fn split_skill(text: &str) -> (Option<String>, String) {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('/') {
        let (skill, body) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if !skill.is_empty() {
            return (Some(skill.to_string()), body.trim().to_string());
        }
    }
    (None, text.to_string())
}

/// The text a job's skill and message read as in the form.
fn join_skill(skill: Option<&str>, body: &str) -> String {
    match skill {
        Some(skill) if body.is_empty() => format!("/{skill}"),
        Some(skill) => format!("/{skill} {body}"),
        None => body.to_string(),
    }
}

/// The next times `repeat` runs after `now`, in `now`'s time zone:
/// `today 16:00, 17:00, 18:00`. A time names its day when the day
/// changes: today, tomorrow, a weekday, or a date further on.
pub(crate) fn next_times<Tz: TimeZone>(repeat: &Repeat, now: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let today = now.date_naive();
    let mut at = now;
    let mut last_day = None;
    let mut parts = Vec::with_capacity(NEXT_COUNT);
    for _ in 0..NEXT_COUNT {
        at = next_after(repeat, at);
        let date = at.date_naive();
        let clock = at.format("%H:%M").to_string();
        if last_day == Some(date) {
            parts.push(clock);
            continue;
        }
        last_day = Some(date);
        let day = match (date - today).num_days() {
            0 => "today".to_string(),
            1 => "tomorrow".to_string(),
            n if n <= WEEKDAY_DAYS => at.format("%a").to_string(),
            _ => at.format("%a %-d %b").to_string(),
        };
        parts.push(format!("{day} {clock}"));
    }
    parts.join(", ")
}

impl JobForm {
    fn new(
        editing: Option<StoredJob>,
        start: Start,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let mut input = |value: String, cx: &mut Context<Workspace>| {
            cx.new(|cx| InputState::new(window, cx).default_value(value))
        };
        let name = input(start.name, cx);
        let (mut kind, mut every, mut only_on) = (RepeatKind::Every, 60, false);
        let (mut every_days, mut weekly_days) = (WEEKDAYS, WEEKDAYS);
        let (mut from, mut to, mut at) = (time(9, 0), time(19, 0), time(9, 0));
        match start.repeat {
            Repeat::Every {
                minutes,
                days,
                window,
            } => {
                every = minutes;
                only_on = days.is_some() || window.is_some();
                if let Some(days) = days {
                    every_days = days_of(days);
                }
                if let Some((start, end)) = window {
                    (from, to) = (start, end);
                }
            }
            Repeat::Daily { at: time } => {
                kind = RepeatKind::Daily;
                at = time;
            }
            Repeat::Weekly { days, at: time } => {
                kind = RepeatKind::Weekly;
                weekly_days = days_of(days);
                at = time;
            }
        }
        let from = input(time_text(from), cx);
        let to = input(time_text(to), cx);
        let at = input(time_text(at), cx);
        let what_text = join_skill(start.skill.as_deref(), &start.body);
        let what = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .default_value(what_text)
        });
        Self {
            editing,
            name,
            what,
            agent: start.agent,
            channel: start.channel,
            kind,
            every,
            only_on,
            every_days,
            from,
            to,
            at,
            weekly_days,
            tell: start.tell,
            on_ask: start.on_ask,
            missed: start.missed,
        }
    }

    /// The job being edited, if one is.
    pub(crate) fn editing(&self) -> Option<JobId> {
        self.editing.as_ref().map(|j| j.id)
    }

    /// Whether it has a name and something to do, so Create can be used.
    pub(crate) fn ready(&self, cx: &App) -> bool {
        let (skill, body) = split_skill(&self.what.read(cx).value());
        !self.name.read(cx).value().trim().is_empty() && (skill.is_some() || !body.is_empty())
    }

    /// How the job repeats, as picked.
    fn repeat(&self, cx: &App) -> Result<Repeat, &'static str> {
        let at = || parse_time(&self.at.read(cx).value()).ok_or(BAD_AT);
        match self.kind {
            RepeatKind::Every if !self.only_on => Ok(Repeat::Every {
                minutes: self.every,
                days: None,
                window: None,
            }),
            RepeatKind::Every => {
                if !self.every_days.contains(&true) {
                    return Err(NO_DAYS);
                }
                let from = parse_time(&self.from.read(cx).value()).ok_or(BAD_HOURS)?;
                let to = parse_time(&self.to.read(cx).value()).ok_or(BAD_HOURS)?;
                Ok(Repeat::Every {
                    minutes: self.every,
                    days: Some(days_set(&self.every_days)),
                    window: Some((from, to)),
                })
            }
            RepeatKind::Daily => Ok(Repeat::Daily { at: at()? }),
            RepeatKind::Weekly => {
                if !self.weekly_days.contains(&true) {
                    return Err(NO_DAYS);
                }
                Ok(Repeat::Weekly {
                    days: days_set(&self.weekly_days),
                    at: at()?,
                })
            }
        }
    }

    /// The job as the form has it, or what to fix first.
    pub(crate) fn new_job(&self, cx: &App) -> Result<NewJob, String> {
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return Err(NO_NAME.into());
        }
        let (skill, body) = split_skill(&self.what.read(cx).value());
        if skill.is_none() && body.is_empty() {
            return Err(NO_MESSAGE.into());
        }
        let agent = self.agent.ok_or(NO_AGENT)?;
        let channel = self.channel.ok_or(NO_CHANNEL)?;
        let repeat = self.repeat(cx)?;
        let editing = self.editing.as_ref();
        Ok(NewJob {
            name,
            body,
            skill,
            model_override: editing.and_then(|j| j.model_override.clone()),
            agent,
            channel,
            repeat,
            tell: self.tell,
            on_ask: self.on_ask,
            missed: self.missed,
            enabled: editing.is_none_or(|j| j.enabled),
        })
    }

    /// What Create or Save sends: the new job, or only what changed.
    pub(crate) fn request(&self, cx: &App) -> Result<Request, String> {
        let job = self.new_job(cx)?;
        let Some(was) = &self.editing else {
            return Ok(Request::CreateJob { job });
        };
        let patch = JobPatch {
            name: changed(&job.name, &was.name),
            body: changed(&job.body, &was.body),
            skill: changed(&job.skill, &was.skill),
            model_override: None,
            agent: changed(&job.agent, &was.agent),
            repeat: changed(&job.repeat, &was.repeat),
            tell: changed(&job.tell, &was.tell),
            on_ask: changed(&job.on_ask, &was.on_ask),
            missed: changed(&job.missed, &was.missed),
        };
        Ok(Request::UpdateJob { job: was.id, patch })
    }
}

impl Workspace {
    /// Opens the job form: `job` to edit, or a new job, filled from
    /// `prefill`.
    pub(crate) fn open_job_form(
        &mut self,
        job: Option<JobId>,
        prefill: Option<JobPrefill>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state();
        let (editing, start) = match job {
            Some(job) => {
                let Some(stored) = state.job(job).cloned() else {
                    return;
                };
                let start = Start::of(&stored);
                (Some(stored), start)
            }
            None => {
                let agent = state.agents().first().map(|a| a.id);
                let channel = self
                    .nav
                    .channel
                    .filter(|c| state.channel(*c).is_some())
                    .or_else(|| {
                        state
                            .projects()
                            .iter()
                            .find_map(|p| state.channels(p.id).first().map(|c| c.id))
                    });
                let start = match prefill {
                    Some(prefill) => Start::prefilled(prefill, agent, channel),
                    None => Start::blank(agent, channel),
                };
                (None, start)
            }
        };
        let form = JobForm::new(editing, start, window, cx);
        let name = form.name.clone();
        self.show_dialog(DialogKind::Job(Box::new(form)), window, cx);
        // After the dialog, and the button that opened it, took focus.
        cx.defer_in(window, move |_, window, cx| {
            name.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    /// The open job form, if the dialog on screen is one.
    pub(crate) fn job_form(&self) -> Option<&JobForm> {
        match self.dialog.as_ref().map(|d| &d.kind) {
            Some(DialogKind::Job(form)) => Some(form),
            _ => None,
        }
    }

    fn job_form_mut(&mut self) -> Option<&mut JobForm> {
        match self.dialog.as_mut().map(|d| &mut d.kind) {
            Some(DialogKind::Job(form)) => Some(form),
            _ => None,
        }
    }

    fn edit_job_form(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut JobForm)) {
        if let Some(form) = self.job_form_mut() {
            change(form);
            cx.notify();
        }
    }

    /// Test run now: creates the job turned off, runs it once and opens
    /// its thread.
    pub(super) fn test_run_job(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.dialog else {
            return;
        };
        let DialogKind::Job(form) = &open.kind else {
            return;
        };
        if open.busy || form.editing.is_some() || !form.ready(cx) {
            return;
        }
        let job = match form.new_job(cx) {
            Ok(job) => job,
            Err(error) => {
                if let Some(open) = &mut self.dialog {
                    open.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        let answer = self.client.request(Request::TestJob { job });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = answer
                .await
                .and_then(|v| serde_json::from_value::<JobId>(v).map_err(|e| e.to_string()));
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(job) => {
                        this.dialog = None;
                        window.close_dialog(cx);
                        this.open_job_thread_when_known(job, window, cx);
                    }
                    Err(error) => {
                        if let Some(open) = &mut this.dialog {
                            open.busy = false;
                            open.error = Some(error);
                            open.task = None;
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(open) = &mut self.dialog {
            open.busy = true;
            open.error = None;
            open.task = Some(task);
        }
        cx.notify();
    }

    /// The form's footer buttons' state: whether Create and Test run can
    /// be used, and whether this is a new job.
    pub(super) fn job_form_ready(&self, cx: &App) -> Option<(bool, bool)> {
        self.job_form().map(|f| (f.ready(cx), f.editing.is_none()))
    }

    pub(super) fn job_form_body(
        &self,
        form: &JobForm,
        this: WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let title = if form.editing.is_some() {
            EDIT_TITLE
        } else {
            NEW_TITLE
        };
        div()
            .id("job-form")
            .flex()
            .flex_col()
            .child(
                div()
                    .id("job-title")
                    .pb(scale::row(8.))
                    .text_size(scale::text(16.))
                    .line_height(scale::line(24.))
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .aria_label(title)
                    .child(title)
                    .test_support(),
            )
            .child(row(
                "Name",
                None,
                true,
                div()
                    .id("job-name")
                    .child(Input::new(&form.name).aria_label("Name"))
                    .test_support(),
            ))
            .child(self.what_field(form, cx))
            .child(self.who_field(form, this.clone()))
            .child(self.repeats_field(form, this.clone(), cx))
            .child(row(
                "Tell me",
                Some("Every run is kept in the thread. Quiet runs are folded."),
                false,
                radio_group(vec![
                    radio_row(
                        "job-tell-changes",
                        "When something changes",
                        "The agent decides. A run with nothing new doesn't mark the thread unread.",
                        form.tell == Tell::Changes,
                        on_form(&this, |f| f.tell = Tell::Changes),
                    ),
                    radio_row(
                        "job-tell-every-run",
                        "After every run",
                        "For reports, like a weekly summary.",
                        form.tell == Tell::EveryRun,
                        on_form(&this, |f| f.tell = Tell::EveryRun),
                    ),
                ]),
            ))
            .child(row(
                "If it needs your OK",
                Some("Nobody is there to approve at 3:00."),
                false,
                radio_group(vec![
                    radio_row(
                        "job-ask-wait",
                        "Wait for me",
                        "The request waits in the thread. Later runs are skipped until you answer.",
                        form.on_ask == OnAsk::Wait,
                        on_form(&this, |f| f.on_ask = OnAsk::Wait),
                    ),
                    radio_row(
                        "job-ask-deny",
                        "Deny it and carry on",
                        "The agent does what it can without it and says what it couldn't do.",
                        form.on_ask == OnAsk::Deny,
                        on_form(&this, |f| f.on_ask = OnAsk::Deny),
                    ),
                ]),
            ))
            .child(row(
                "Missed runs",
                Some("Crew can't run while the Mac sleeps."),
                false,
                div().flex().child(segmented(vec![
                    segment(
                        "job-missed-wake",
                        "Run once on wake",
                        form.missed == Missed::OnceOnWake,
                        on_form(&this, |f| f.missed = Missed::OnceOnWake),
                    ),
                    segment(
                        "job-missed-skip",
                        "Skip them",
                        form.missed == Missed::Skip,
                        on_form(&this, |f| f.missed = Missed::Skip),
                    ),
                ])),
            ))
            .test_support()
            .into_any_element()
    }

    fn what_field(&self, form: &JobForm, cx: &App) -> AnyElement {
        let (skill, _) = split_skill(&form.what.read(cx).value());
        let runtime = form
            .agent
            .and_then(|a| self.state().agent(a))
            .map_or(Runtime::ClaudeCode, |a| a.spec.runtime.clone());
        let under = match skill {
            Some(skill) => div()
                .flex()
                .child(skill_chip("job-skill", &runtime, &skill).test_support())
                .into_any_element(),
            None => div()
                .flex()
                .gap(ui_px(4.))
                .text_size(scale::text(12.))
                .text_color(color("text-tertiary"))
                .child("Type")
                .child(
                    div()
                        .font_family(code_family())
                        .text_color(color("text-secondary"))
                        .child("/"),
                )
                .child("to run a skill instead.")
                .into_any_element(),
        };
        row(
            "What to do",
            Some("Sent to the agent as the message, every run."),
            false,
            div()
                .flex()
                .flex_col()
                .gap(ui_px(6.))
                .child(
                    div()
                        .id("job-what")
                        .child(Textarea::new(&form.what).aria_label("What to do"))
                        .test_support(),
                )
                .child(under),
        )
        .into_any_element()
    }

    fn who_field(&self, form: &JobForm, this: WeakEntity<Self>) -> AnyElement {
        let state = self.state();
        let agent = form.agent.and_then(|a| state.agent(a));
        let agents: Vec<(AgentId, String)> = state
            .agents()
            .iter()
            .map(|a| (a.id, a.spec.name.clone()))
            .collect();
        let picked = form.agent;
        let agent_this = this.clone();
        let agent_button = Button::new("job-agent")
            .outline()
            .accessibility_label(format!(
                "Agent: {}",
                agent.map_or("none", |a| a.spec.name.as_str())
            ))
            .when_some(agent, |b, a| {
                b.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(ui_px(8.))
                        .child(monogram(&a.spec.name, 18.))
                        .child(a.spec.name.clone())
                        .child(chip(runtime_chip(&a.spec))),
                )
            })
            .when(agent.is_none(), |b| b.label("Pick an agent"))
            .icon(IconName::ChevronDown)
            .dropdown_menu(move |menu, _, _| {
                agents.iter().fold(menu, |menu, (id, name)| {
                    let this = agent_this.clone();
                    let id = *id;
                    menu.item(
                        PopupMenuItem::new(name.clone())
                            .checked(picked == Some(id))
                            .on_click(move |_, _, cx| {
                                this.update(cx, |ws, cx| {
                                    ws.edit_job_form(cx, |f| f.agent = Some(id));
                                })
                                .ok();
                            }),
                    )
                })
            });
        let channel_text = |channel: ChannelId| {
            state.channel(channel).map(|c| {
                let project = state
                    .project(c.project)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                (project, c.name.clone())
            })
        };
        let picked_channel = form.channel.and_then(channel_text);
        let channel_face = |(project, name): &(String, String)| {
            div()
                .flex()
                .items_center()
                .gap(ui_px(8.))
                .child(project.clone())
                .child(div().text_color(color("indent-guide-active")).child("/"))
                .child(
                    div()
                        .flex()
                        .child(
                            div()
                                .font_family(code_family())
                                .text_color(color("text-tertiary"))
                                .child("#"),
                        )
                        .child(name.clone()),
                )
        };
        let channel_label = picked_channel
            .as_ref()
            .map_or_else(String::new, |(p, n)| format!("{p} / #{n}"));
        let channel = if form.editing.is_some() {
            // The channel can't change once the job is made.
            div()
                .id("job-channel-fixed")
                .flex()
                .items_center()
                .h(scale::row(32.))
                .px(ui_px(10.))
                .border_1()
                .border_color(color("border-100"))
                .rounded(ui_px(6.))
                .text_color(color("text-secondary"))
                .aria_label(channel_label)
                .children(picked_channel.as_ref().map(channel_face))
                .test_support()
                .into_any_element()
        } else {
            let mut channels = Vec::new();
            for project in state.projects() {
                for c in state.channels(project.id) {
                    channels.push((c.id, format!("{} / #{}", project.name, c.name)));
                }
            }
            let picked = form.channel;
            Button::new("job-channel")
                .outline()
                .accessibility_label(format!("Channel: {channel_label}"))
                .when_some(picked_channel.as_ref(), |b, c| b.child(channel_face(c)))
                .when(picked_channel.is_none(), |b| b.label("Pick a channel"))
                .icon(IconName::ChevronDown)
                .dropdown_menu(move |menu, _, _| {
                    channels.iter().fold(menu, |menu, (id, text)| {
                        let this = this.clone();
                        let id = *id;
                        menu.item(
                            PopupMenuItem::new(text.clone())
                                .checked(picked == Some(id))
                                .on_click(move |_, _, cx| {
                                    this.update(cx, |ws, cx| {
                                        ws.edit_job_form(cx, |f| f.channel = Some(id));
                                    })
                                    .ok();
                                }),
                        )
                    })
                })
                .into_any_element()
        };
        row(
            "Who and where",
            Some("Runs use this agent's permissions and model."),
            false,
            div()
                .flex()
                .flex_wrap()
                .gap(ui_px(8.))
                .child(agent_button)
                .child(channel),
        )
        .into_any_element()
    }

    fn repeats_field(&self, form: &JobForm, this: WeakEntity<Self>, cx: &App) -> AnyElement {
        let kinds = [
            ("job-repeat-every", "Every few hours", RepeatKind::Every),
            ("job-repeat-daily", "Daily", RepeatKind::Daily),
            ("job-repeat-weekly", "Weekly", RepeatKind::Weekly),
        ]
        .map(|(id, text, kind)| {
            segment(
                id,
                text,
                form.kind == kind,
                on_form(&this, move |f| f.kind = kind),
            )
        });
        let detail = match form.kind {
            RepeatKind::Every => self.every_detail(form, this.clone()),
            RepeatKind::Daily => div()
                .flex()
                .items_center()
                .gap(ui_px(8.))
                .child("At")
                .child(time_input("job-at", &form.at))
                .into_any_element(),
            RepeatKind::Weekly => div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(ui_px(8.))
                .child(day_buttons(&form.weekly_days, true, this.clone(), |f| {
                    &mut f.weekly_days
                }))
                .child("at")
                .child(time_input("job-at", &form.at))
                .into_any_element(),
        };
        let next = form.repeat(cx).ok().map(|repeat| {
            let times = next_times(&repeat, Local::now());
            div()
                .id("job-next")
                .flex()
                .gap(ui_px(4.))
                .text_size(scale::text(12.))
                .text_color(color("text-tertiary"))
                .aria_label(format!("Next: {times}"))
                .child("Next:")
                .child(
                    div()
                        .font_family(code_family())
                        .text_color(color("text-secondary"))
                        .child(times),
                )
                .test_support()
        });
        row(
            "Repeats",
            None,
            false,
            div()
                .flex()
                .flex_col()
                .gap(ui_px(10.))
                .child(div().flex().child(segmented(kinds.into())))
                .child(detail)
                .children(next),
        )
        .into_any_element()
    }

    fn every_detail(&self, form: &JobForm, this: WeakEntity<Self>) -> AnyElement {
        let mut intervals = INTERVALS.to_vec();
        if !intervals.contains(&form.every) {
            intervals.push(form.every);
            intervals.sort_unstable();
        }
        let every = form.every;
        let interval_this = this.clone();
        let interval = Button::new("job-interval")
            .outline()
            .label(interval_label(every))
            .icon(IconName::ChevronDown)
            .dropdown_menu(move |menu, _, _| {
                intervals.iter().fold(menu, |menu, minutes| {
                    let this = interval_this.clone();
                    let minutes = *minutes;
                    menu.item(
                        PopupMenuItem::new(interval_label(minutes))
                            .checked(minutes == every)
                            .on_click(move |_, _, cx| {
                                this.update(cx, |ws, cx| {
                                    ws.edit_job_form(cx, |f| f.every = minutes);
                                })
                                .ok();
                            }),
                    )
                })
            });
        let on = form.only_on;
        let toggle_label = if on { "Only on (on)" } else { "Only on" };
        div()
            .flex()
            .flex_col()
            .gap(ui_px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .child("Every")
                    .child(
                        div()
                            .id("job-interval-value")
                            .aria_label(interval_label(every))
                            .child(interval)
                            .test_support(),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(ui_px(8.))
                    .text_color(color("text-secondary"))
                    .child(
                        div()
                            .id("job-only-on")
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .cursor_pointer()
                            .role(Role::Switch)
                            .aria_label(toggle_label)
                            .child(switch(on))
                            .child("Only on")
                            .on_click({
                                let this = this.clone();
                                move |_, _, cx| {
                                    this.update(cx, |ws, cx| {
                                        ws.edit_job_form(cx, |f| f.only_on = !f.only_on);
                                    })
                                    .ok();
                                }
                            })
                            .test_support(),
                    )
                    .child(day_buttons(&form.every_days, on, this, |f| {
                        &mut f.every_days
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(6.))
                            .when(!on, |d| d.opacity(0.5))
                            .child("from")
                            .child(time_input_off("job-from", &form.from, !on))
                            .child("to")
                            .child(time_input_off("job-to", &form.to, !on)),
                    ),
            )
            .into_any_element()
    }
}

/// Calls `change` on the open job form.
fn on_form(
    this: &WeakEntity<Workspace>,
    change: impl Fn(&mut JobForm) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let this = this.clone();
    move |_, _, cx| {
        this.update(cx, |ws, cx| ws.edit_job_form(cx, &change)).ok();
    }
}

/// One of the form's rows: its label and help on the left, the control
/// on the right, a line above all but the first. The first, Name, sets
/// its label level with the field's text.
fn row(
    label: &'static str,
    help: Option<&'static str>,
    first: bool,
    content: impl IntoElement,
) -> Div {
    div()
        .flex()
        .gap(ui_px(20.))
        .py(scale::row(14.))
        .when(!first, |r| r.border_t_1().border_color(color("border-100")))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .w(ui_px(LABEL_COL))
                .when(first, |c| c.pt(scale::row(7.)))
                .child(
                    div()
                        .text_size(scale::text(13.))
                        .font_weight(weight(FontWeight::MEDIUM))
                        .child(label),
                )
                .children(help.map(|help| {
                    div()
                        .mt(ui_px(2.))
                        .text_size(scale::text(12.))
                        .line_height(scale::line(16.))
                        .text_color(color("text-tertiary"))
                        .child(help)
                })),
        )
        .child(div().flex_1().min_w_0().child(content))
}

/// A segmented control's frame.
fn segmented(children: Vec<AnyElement>) -> Div {
    div()
        .flex()
        .gap(ui_px(2.))
        .p(ui_px(2.))
        .border_1()
        .border_color(color("border-200"))
        .rounded(ui_px(6.))
        .children(children)
}

/// One option of a segmented control.
fn segment(
    id: &'static str,
    text: &'static str,
    selected: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(scale::row(26.))
        .px(ui_px(12.))
        .rounded(ui_px(4.))
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .when(selected, |s| s.bg(color("surface-300")))
        .text_color(color(if selected {
            "text-primary"
        } else {
            "text-secondary"
        }))
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(if selected {
            format!("{text} (selected)")
        } else {
            text.to_string()
        })
        .child(text)
        .on_click(click)
        .test_support()
        .into_any_element()
}

fn radio_group(rows: Vec<AnyElement>) -> Div {
    div().flex().flex_col().gap(ui_px(6.)).children(rows)
}

/// A radio row: the option in medium weight, what it means under it.
fn radio_row(
    id: &'static str,
    title: &'static str,
    text: &'static str,
    selected: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_start()
        .gap(ui_px(10.))
        .px(ui_px(12.))
        .py(scale::row(8.))
        .border_1()
        .rounded(ui_px(6.))
        .border_color(color(if selected { "border-200" } else { "border-100" }))
        .when(selected, |r| r.bg(color("surface-100")))
        .cursor_pointer()
        .role(Role::RadioButton)
        .aria_label(if selected {
            format!("{title} (selected)")
        } else {
            title.to_string()
        })
        .child(
            div()
                .flex_none()
                .mt(ui_px(3.))
                .size(ui_px(13.))
                .rounded_full()
                .border_1()
                .border_color(color(if selected {
                    "text-primary"
                } else {
                    "border-200"
                }))
                .when(selected, |dot| {
                    dot.flex().items_center().justify_center().child(
                        div()
                            .size(ui_px(7.))
                            .rounded_full()
                            .bg(color("text-primary")),
                    )
                }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .child(div().font_weight(weight(FontWeight::MEDIUM)).child(title))
                .child(
                    div()
                        .text_size(scale::text(12.))
                        .text_color(color("text-tertiary"))
                        .child(text),
                ),
        )
        .on_click(click)
        .test_support()
        .into_any_element()
}

/// The week's day buttons, M to S, each on or off. When `enabled` is
/// false they are dimmed and do nothing.
fn day_buttons(
    days: &[bool; 7],
    enabled: bool,
    this: WeakEntity<Workspace>,
    pick: fn(&mut JobForm) -> &mut [bool; 7],
) -> Div {
    div()
        .flex()
        .gap(ui_px(3.))
        .when(!enabled, |d| d.opacity(0.5))
        .children(WEEK.iter().enumerate().map(|(ix, day)| {
            let on = days[ix];
            let letter = &day.to_string()[..1];
            let name = day_name(*day);
            let this = this.clone();
            div()
                .id(SharedString::from(format!("job-day-{ix}")))
                .flex()
                .items_center()
                .justify_center()
                .w(ui_px(30.))
                .h(scale::row(26.))
                .border_1()
                .rounded(ui_px(4.))
                .text_size(scale::text(12.))
                .border_color(color(if on {
                    "indent-guide-active"
                } else {
                    "border-200"
                }))
                .when(on, |b| b.bg(color("surface-300")))
                .text_color(color(if on { "text-primary" } else { "text-secondary" }))
                .when(enabled, |b| b.cursor_pointer())
                .role(Role::Button)
                .aria_label(if on {
                    format!("{name} (on)")
                } else {
                    name.to_string()
                })
                .child(letter.to_string())
                .on_click(move |_, _, cx| {
                    if !enabled {
                        return;
                    }
                    this.update(cx, |ws, cx| {
                        ws.edit_job_form(cx, |f| {
                            let days = pick(f);
                            days[ix] = !days[ix];
                        });
                    })
                    .ok();
                })
                .test_support()
        }))
}

fn day_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

/// A short field for a time, such as `09:00`.
fn time_input(id: &'static str, state: &Entity<InputState>) -> impl IntoElement {
    time_input_off(id, state, false)
}

fn time_input_off(id: &'static str, state: &Entity<InputState>, off: bool) -> impl IntoElement {
    div()
        .id(id)
        .w(ui_px(72.))
        .font_family(code_family())
        .child(Input::new(state).small().disabled(off))
        .test_support()
}

#[cfg(test)]
mod tests {
    use super::{interval_label, join_skill, split_skill};

    #[test]
    fn a_leading_slash_names_the_skill() {
        assert_eq!(
            split_skill(" /unslop  the docs "),
            (Some("unslop".into()), "the docs".into())
        );
        assert_eq!(
            split_skill("/unslop"),
            (Some("unslop".into()), String::new())
        );
        assert_eq!(split_skill("check it"), (None, "check it".into()));
        assert_eq!(split_skill("/ nothing"), (None, "/ nothing".into()));
        assert_eq!(join_skill(Some("unslop"), "the docs"), "/unslop the docs");
        assert_eq!(join_skill(Some("unslop"), ""), "/unslop");
        assert_eq!(join_skill(None, "check it"), "check it");
    }

    #[test]
    fn intervals_read_as_the_menu_says() {
        assert_eq!(
            [15, 30, 60, 120, 360].map(interval_label),
            ["15 min", "30 min", "hour", "2 hours", "6 hours"]
        );
    }
}
