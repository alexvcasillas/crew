//! A scheduled job's thread, as `V11ScheduleThread` draws it: the
//! header's repeat tag and Run now, the setup message's tag, quiet runs
//! folded into one line, the found reply's tag, the composer's hint, and
//! the notification for a run that found something.

use crew_core::event::RunStatus;
use crew_core::ids::{JobId, MessageId, RunId, ThreadId};
use crew_core::schedule::describe;
use crew_core::store::{Author, StoredJob, StoredMessage};
use crew_proto::Request;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Icon, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{clock, monogram, now_ms};
use super::job_runs::{Tick, day_time, first_line, local_format, same_day, tick_mark};
use super::scheduled_view::CLOCK_SVG;
use super::workspace::Workspace;
use crate::model::State;
use crate::theme::color;
use crate::theme::scale::{self, ui_px, weight};

/// The composer's hint in a job's thread.
pub const JOB_HINT: &str = "A reply runs once. To change every run, edit the job.";

/// How a message of a thread shows, given the quiet replies around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Quiet {
    /// As a message.
    Full,
    /// As one line: the latest quiet run, or one alone.
    OneLiner,
    /// The first of quiet replies folded into one line.
    Fold(Fold),
    /// Folded into the line the quiet reply `.0` starts.
    Folded(MessageId),
}

/// Quiet replies folded into one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Fold {
    pub first: MessageId,
    pub count: usize,
    /// Unix ms of the first and last.
    pub from: i64,
    pub to: i64,
}

/// How each of `messages` shows. Quiet replies in a row fold into one
/// line; at the end of the thread the latest stays out as a one-liner,
/// and one alone is a one-liner too.
pub(super) fn quiet_plan(messages: &[StoredMessage]) -> Vec<Quiet> {
    let mut plan = vec![Quiet::Full; messages.len()];
    let mut at = 0;
    while at < messages.len() {
        if !messages[at].quiet {
            at += 1;
            continue;
        }
        let start = at;
        while at < messages.len() && messages[at].quiet {
            at += 1;
        }
        let mut end = at;
        if end == messages.len() {
            end -= 1;
            plan[end] = Quiet::OneLiner;
        }
        match end - start {
            0 => {}
            1 => plan[start] = Quiet::OneLiner,
            count => {
                let first = messages[start].id;
                plan[start] = Quiet::Fold(Fold {
                    first,
                    count,
                    from: messages[start].created_at,
                    to: messages[end - 1].created_at,
                });
                for slot in &mut plan[start + 1..end] {
                    *slot = Quiet::Folded(first);
                }
            }
        }
    }
    plan
}

/// "18 runs with nothing new · yesterday 20:00 to today 13:00".
pub(super) fn fold_text(fold: &Fold, now: i64) -> String {
    let runs = if fold.count == 1 { "run" } else { "runs" };
    let to = if same_day(fold.from, fold.to) {
        local_format(fold.to, "%H:%M")
    } else {
        day_time(fold.to, now)
    };
    format!(
        "{} {runs} with nothing new · {} to {to}",
        fold.count,
        day_time(fold.from, now)
    )
}

/// "Every hour · next 16:00", or "Every hour · paused".
pub(super) fn header_tag(job: &StoredJob, now: i64) -> String {
    let (repeat, _) = describe(&job.repeat);
    match job.next_due.filter(|_| job.enabled) {
        Some(due) if same_day(due, now) => {
            format!("{repeat} · next {}", local_format(due, "%H:%M"))
        }
        Some(due) => format!("{repeat} · next {}", day_time(due, now)),
        None => format!("{repeat} · paused"),
    }
}

/// "every hour" from "Every hour"; a repeat led by a day keeps its case.
fn repeat_words(job: &StoredJob) -> String {
    let (repeat, _) = describe(&job.repeat);
    match repeat.strip_prefix("Every ") {
        Some(rest) => format!("every {rest}"),
        None => repeat,
    }
}

/// The tag and the time a job message shows: the first is the job's
/// setup, the later ones its edits.
pub(super) fn job_message_head(
    state: &State,
    message: &StoredMessage,
) -> Option<(&'static str, Option<String>)> {
    if !message.job_message {
        return None;
    }
    let first = state
        .messages(message.thread)
        .iter()
        .find(|m| m.job_message)
        .map(|m| m.id);
    if first != Some(message.id) {
        return Some(("Scheduled job · edited", None));
    }
    let set_up = local_format(message.created_at, "%-d %b");
    let time = match state.thread_job(message.thread) {
        Some(job) => format!("Set up {set_up} · {}", repeat_words(job)),
        None => format!("Set up {set_up}"),
    };
    Some(("Scheduled job", Some(time)))
}

/// `body` without the `/skill` it starts with, as an edited job message
/// does, so a run's skill chip doesn't show it twice.
pub(super) fn without_skill<'a>(body: &'a str, skill: &str) -> &'a str {
    let Some(rest) = body.strip_prefix('/').and_then(|b| b.strip_prefix(skill)) else {
        return body;
    };
    if rest.is_empty() {
        rest
    } else if let Some(rest) = rest.strip_prefix(' ') {
        rest
    } else {
        body
    }
}

/// Whether `message` is the reply of a job's run that found something.
pub(super) fn found_reply(state: &State, message: &StoredMessage) -> bool {
    matches!(message.author, Author::Agent(_))
        && message
            .run
            .and_then(|r| state.run(r))
            .is_some_and(|r| r.job.is_some() && r.found == Some(true))
}

/// The boards' `.tag`: small grey text in a box, or green for news,
/// after `icon` if given.
pub(super) fn tag(
    id: String,
    text: String,
    news: bool,
    icon: Option<&'static [u8]>,
) -> Stateful<Div> {
    let (fg, bg, border) = if news {
        ("success", "success-subtle", "success-border")
    } else {
        ("text-tertiary", "surface-200", "border-100")
    };
    div()
        .id(SharedString::from(id))
        .flex()
        .flex_none()
        .items_center()
        .gap(ui_px(4.))
        .px(ui_px(6.))
        .rounded(ui_px(4.))
        .border_1()
        .border_color(color(border))
        .bg(color(bg))
        .text_color(color(fg))
        .text_size(scale::text(11.))
        .line_height(scale::line(16.))
        .font_weight(weight(FontWeight::NORMAL))
        .whitespace_nowrap()
        .aria_label(text.clone())
        .when_some(icon, |tag, svg| {
            tag.child(Icon::default().data(svg).size(ui_px(11.)))
        })
        .child(text)
}

impl Workspace {
    /// What a job's thread's header shows: how the job repeats and when
    /// it runs next, by the title, and Run now.
    pub(super) fn render_job_header(
        &self,
        job: &StoredJob,
        cx: &mut Context<Self>,
    ) -> (AnyElement, AnyElement) {
        let id = job.id;
        let tag = tag(
            "thread-job-tag".into(),
            header_tag(job, now_ms()),
            false,
            Some(CLOCK_SVG),
        )
        .test_support()
        .into_any_element();
        let run_now = Button::new("thread-run-now")
            .outline()
            .small()
            .label("Run now")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.request(Request::RunJobNow { job: id }, window, cx);
            }))
            .into_any_element();
        (tag, run_now)
    }

    /// Quiet replies folded into one line, with Show.
    pub(super) fn render_fold(&self, fold: &Fold, cx: &mut Context<Self>) -> AnyElement {
        let first = fold.first;
        let open = self.job_folds_open.contains(&first);
        let text = fold_text(fold, now_ms());
        let toggle = if open { "Hide" } else { "Show" };
        div()
            .id(SharedString::from(format!("job-fold-{first}")))
            .flex()
            .items_center()
            .gap(ui_px(10.))
            .mx(scale::chrome(24.))
            .ml(scale::chrome(24.) + ui_px(32. + 12.))
            .my(scale::row(4.))
            .px(ui_px(10.))
            .py(scale::row(6.))
            .border_1()
            .border_dashed()
            .border_color(color("border-200"))
            .rounded(ui_px(6.))
            .text_size(scale::text(12.))
            .text_color(color("text-tertiary"))
            .aria_label(text.clone())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(ui_px(2.))
                    .children((0..fold.count.min(FOLD_TICKS)).map(|_| tick_mark(Tick::Quiet))),
            )
            .child(div().flex_1().min_w_0().truncate().child(text))
            .child(
                div()
                    .id(SharedString::from(format!("job-fold-show-{first}")))
                    .flex_none()
                    .cursor_pointer()
                    .text_color(color("accent-blue"))
                    .aria_label(toggle)
                    .child(toggle)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.job_folds_open.remove(&first) {
                            this.job_folds_open.insert(first);
                        }
                        cx.notify();
                    }))
                    .test_support(),
            )
            .test_support()
            .into_any_element()
    }

    /// A quiet reply on one line: who, when and its first line.
    pub(super) fn render_quiet(&self, message: &StoredMessage) -> AnyElement {
        let name = match message.author {
            Author::Agent(a) => self
                .state()
                .agent(a)
                .map_or_else(|| "Deleted agent".to_string(), |a| a.spec.name.clone()),
            _ => String::new(),
        };
        let line = format!(
            "{} · {}",
            clock(message.created_at),
            first_line(&message.body)
        );
        div()
            .id(SharedString::from(format!("quiet-{}", message.id)))
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .pl(scale::chrome(24.) + ui_px(32. + 12.))
            .pr(scale::chrome(24.))
            .py(scale::row(6.))
            .text_size(scale::text(12.))
            .text_color(color("text-tertiary"))
            .whitespace_nowrap()
            .aria_label(format!("{name} · {line}"))
            .child(monogram(&name, 18.))
            .child(
                div()
                    .flex_none()
                    .text_color(color("text-secondary"))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .child(name),
            )
            .child(div().min_w_0().truncate().child(line))
            .test_support()
            .into_any_element()
    }

    /// The run of a job that `msg`, about to be taken in, says ended
    /// done, unless it was known to have ended already.
    pub(super) fn job_runs_ending(&self, msg: &crew_proto::ServerMsg) -> Option<RunId> {
        let crew_proto::ServerMsg::RunStatus {
            run,
            status,
            details,
            ..
        } = msg
        else {
            return None;
        };
        let was_live = self
            .state()
            .run(*run)
            .is_none_or(|r| matches!(r.status, RunStatus::Queued | RunStatus::Working));
        (details.job.is_some() && *status == RunStatus::Done && was_live).then_some(*run)
    }

    /// Tells you a job's run found something, unless you are looking at
    /// its thread. Quiet replies never notify.
    pub(super) fn notify_found_run(
        &mut self,
        run: RunId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state();
        let Some(r) = state.run(run) else {
            return;
        };
        let Some(job) = r.job.and_then(|j| state.job(j)) else {
            return;
        };
        if !State::job_run_found(job, r) || self.viewing_thread(job.thread) {
            return;
        }
        let reply = state
            .messages(job.thread)
            .iter()
            .rev()
            .find(|m| m.run == Some(run) && matches!(m.author, Author::Agent(_)));
        if reply.is_some_and(|m| m.quiet) {
            return;
        }
        let agent = state
            .agent(r.agent)
            .map_or_else(|| "Agent".to_string(), |a| a.spec.name.clone());
        let line = r
            .summary
            .as_deref()
            .map(str::to_string)
            .or_else(|| reply.map(|m| first_line(&m.body)))
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "Found something".into());
        let notice = job_notice(
            job.id,
            job.thread,
            job.name.clone(),
            format!("{agent}: {line}"),
            cx,
        );
        window.push_notification(notice, cx);
    }

    /// Whether `thread` is open in front of you.
    fn viewing_thread(&self, thread: ThreadId) -> bool {
        self.thread_seen(thread) == i64::MAX
    }
}

/// The fold's strip shows at most this many runs.
const FOLD_TICKS: usize = 24;

/// One notification per job: a newer found run replaces the last.
struct JobNotice;

/// The toast for a found run: the clock, the job's name, the line, and
/// Open, which opens the job's thread.
fn job_notice(
    job: JobId,
    thread: ThreadId,
    title: String,
    text: String,
    cx: &mut Context<Workspace>,
) -> Notification {
    let ws = cx.entity().downgrade();
    Notification::new()
        .id1::<JobNotice>(SharedString::from(job.to_string()))
        .icon(
            Icon::default()
                .data(CLOCK_SVG)
                .text_color(color("text-secondary")),
        )
        .content(move |_, _, _| {
            div()
                .flex()
                .flex_col()
                .gap(ui_px(2.))
                .child(
                    div()
                        .id("job-notice-title")
                        .font_weight(weight(FontWeight::MEDIUM))
                        .aria_label(title.clone())
                        .child(title.clone())
                        .test_support(),
                )
                .child(
                    div()
                        .id("job-notice-text")
                        .text_size(scale::text(12.))
                        .line_height(scale::line(16.))
                        .text_color(color("text-tertiary"))
                        .aria_label(text.clone())
                        .child(text.clone())
                        .test_support(),
                )
                .into_any_element()
        })
        .action(move |_, _, cx| {
            let ws = ws.clone();
            let note = cx.entity().downgrade();
            Button::new("job-notice-open")
                .outline()
                .small()
                .label("Open")
                .on_click(move |_, window, cx| {
                    ws.update(cx, |ws, cx| ws.open_job_thread(thread, window, cx))
                        .ok();
                    note.update(cx, |note, cx| note.dismiss(window, cx)).ok();
                })
        })
}

#[cfg(test)]
mod tests {
    use crew_core::ids::{MessageId, ThreadId};
    use crew_core::store::{Author, StoredMessage};

    use super::{Quiet, quiet_plan};

    fn message(quiet: bool, at: i64) -> StoredMessage {
        StoredMessage {
            id: MessageId::new(),
            thread: ThreadId::new(),
            author: Author::You,
            body: String::new(),
            created_at: at,
            run: None,
            approval: None,
            quiet,
            job_message: false,
        }
    }

    fn kinds(quiet: &[bool]) -> Vec<&'static str> {
        let messages: Vec<StoredMessage> = quiet
            .iter()
            .enumerate()
            .map(|(i, q)| message(*q, i as i64))
            .collect();
        quiet_plan(&messages)
            .iter()
            .map(|q| match q {
                Quiet::Full => "full",
                Quiet::OneLiner => "one",
                Quiet::Fold(_) => "fold",
                Quiet::Folded(_) => "in",
            })
            .collect()
    }

    #[test]
    fn quiet_replies_fold_and_the_latest_stays_out() {
        assert_eq!(
            kinds(&[false, true, true, true, false, true]),
            ["full", "fold", "in", "in", "full", "one"]
        );
        assert_eq!(
            kinds(&[false, true, true, true]),
            ["full", "fold", "in", "one"]
        );
        assert_eq!(
            kinds(&[true, false, true, true]),
            ["one", "full", "one", "one"]
        );
        assert_eq!(kinds(&[false]), ["full"]);
    }
}
