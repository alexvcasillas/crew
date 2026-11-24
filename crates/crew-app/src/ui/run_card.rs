//! The card under a run's message: its last steps, time and file counts
//! while it works, and what it changed once it is done.

use std::collections::HashMap;

use crew_core::claude::parse::classifier_reason;
use crew_core::event::{Event, ToolKind, humanize_reason, tool_label};
use crew_core::ids::RunId;
use crew_core::store::{ApprovalKind, ApprovalStatus, StoredApproval, StoredRun};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::approval_card::outside_sandbox;
use super::common::{elapsed, now_ms, rolling_text, spinner};
use super::inspector::NOTHING_TO_REVIEW;
use super::workspace::Workspace;
use crate::model::LoggedEvent;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// How many of the latest steps a card lists.
const STEPS_SHOWN: usize = 6;
/// Longest step target before it is cut short.
const TARGET_MAX: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Done,
    Running,
    Failed,
    /// Claude Code's auto mode safety check stopped it.
    Blocked,
}

/// One tool call of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub state: StepState,
    pub verb: String,
    pub target: String,
    /// Lines added and removed, for edits and new files.
    pub lines: Option<(u32, u32)>,
    /// Why the safety check stopped it, as a sentence, if it did.
    pub reason: Option<String>,
    /// The tool's own name, when [`tool_label`] reads it another way.
    pub raw: Option<String>,
    /// How you answered the ask to run it, if it asked.
    pub answer: Option<Answer>,
    /// It asked to run outside Claude Code's sandbox.
    pub outside: bool,
}

/// The tag on a step or Activity row that ran outside Claude Code's
/// sandbox.
pub const OUTSIDE_TAG: &str = "outside sandbox";

/// How you answered a permission ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allowed,
    Denied,
}

impl Answer {
    pub fn text(self) -> &'static str {
        match self {
            Answer::Allowed => "allowed by you",
            Answer::Denied => "denied by you",
        }
    }
}

/// A permission ask you answered, to tag the call it was about.
#[derive(Debug, Clone, PartialEq)]
pub struct DecidedAsk {
    pub tool_use_id: Option<String>,
    pub tool: String,
    pub input: serde_json::Value,
    pub answer: Answer,
}

/// `run`'s permission asks that you allowed or denied, oldest first, from
/// a thread's approvals in that order. One no longer needed tags nothing.
pub fn decided_asks(approvals: &[&StoredApproval], run: RunId) -> Vec<DecidedAsk> {
    approvals
        .iter()
        .filter(|a| a.run == run)
        .filter_map(|a| {
            let ApprovalKind::Permission {
                tool,
                input,
                tool_use_id,
            } = &a.kind
            else {
                return None;
            };
            let answer = match a.status {
                ApprovalStatus::Approved => Answer::Allowed,
                ApprovalStatus::Declined => Answer::Denied,
                _ => return None,
            };
            Some(DecidedAsk {
                tool_use_id: tool_use_id.clone(),
                tool: tool.clone(),
                input: input.clone(),
                answer,
            })
        })
        .collect()
}

/// For each of a run's calls, as id, tool and input in order, the answer
/// of the ask about it. An ask finds its call by tool use id, else it
/// takes the first call not yet matched with the same tool and input,
/// else the first with the same tool.
pub fn match_asks(
    calls: &[(&str, &str, &serde_json::Value)],
    asks: &[DecidedAsk],
) -> Vec<Option<Answer>> {
    let mut answers: Vec<Option<Answer>> = vec![None; calls.len()];
    let mut later = Vec::new();
    for ask in asks {
        let by_id = ask
            .tool_use_id
            .as_deref()
            .and_then(|id| calls.iter().position(|c| c.0 == id));
        match by_id {
            Some(ix) => answers[ix] = Some(ask.answer),
            None => later.push(ask),
        }
    }
    for ask in later {
        let free = |ix: &usize| answers[*ix].is_none() && calls[*ix].1 == ask.tool;
        let ix = (0..calls.len())
            .filter(free)
            .find(|ix| *calls[*ix].2 == ask.input)
            .or_else(|| (0..calls.len()).find(free));
        if let Some(ix) = ix {
            answers[ix] = Some(ask.answer);
        }
    }
    answers
}

impl Step {
    /// What the step reads as, such as `Edited src/lib.rs`.
    pub fn text(&self) -> String {
        if self.target.trim().is_empty() {
            self.verb.clone()
        } else {
            format!("{} {}", self.verb, self.target)
        }
    }
}

/// A file the run edited or created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

/// What a run did, from its events.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunSummary {
    /// The latest steps, oldest first.
    pub steps: Vec<Step>,
    /// In the order first touched.
    pub files: Vec<FileChange>,
}

impl RunSummary {
    pub fn added(&self) -> u32 {
        self.files.iter().map(|f| f.added).sum()
    }

    pub fn removed(&self) -> u32 {
        self.files.iter().map(|f| f.removed).sum()
    }

    /// `2 files · +42 −3`, or nothing if no file changed.
    pub fn counts(&self) -> Option<String> {
        let n = self.files.len();
        (n > 0).then(|| {
            let files = if n == 1 { "file" } else { "files" };
            format!("{n} {files} · +{} −{}", self.added(), self.removed())
        })
    }
}

/// A tool call as first seen.
struct Call {
    id: String,
    input: serde_json::Value,
    kind: ToolKind,
    name: String,
    /// Relative to the run's folder.
    path: Option<String>,
    target: String,
    lines: Option<(u32, u32)>,
}

/// Sums up `run`'s events among a thread's.
pub fn summarize(events: &[LoggedEvent], run: RunId) -> RunSummary {
    summarize_asked(events, run, &[])
}

/// Like [`summarize`], each step tagged with your answer if it asked.
pub fn summarize_asked(events: &[LoggedEvent], run: RunId, asks: &[DecidedAsk]) -> RunSummary {
    let mut cwd = String::new();
    let mut calls: Vec<Call> = Vec::new();
    let mut ends: HashMap<String, bool> = HashMap::new();
    let mut blocked: HashMap<String, String> = HashMap::new();
    for logged in events.iter().filter(|e| e.run == run) {
        match &logged.event {
            Event::SessionStarted { cwd: dir, .. } => cwd = dir.clone(),
            Event::ToolCall {
                id,
                name,
                kind,
                target,
                input,
            } => {
                let target = target.as_deref().map(|t| relative(t, &cwd));
                calls.push(Call {
                    id: id.clone(),
                    input: input.clone(),
                    kind: kind.clone(),
                    name: name.clone(),
                    target: shorten(target.as_deref().unwrap_or_default()),
                    path: target,
                    lines: logged.lines,
                });
            }
            Event::ToolResult {
                id,
                is_error,
                output,
            } => {
                ends.insert(id.clone(), *is_error);
                if let Some(reason) = is_error.then(|| classifier_reason(output)).flatten() {
                    blocked.entry(id.clone()).or_insert(reason);
                }
            }
            Event::PermissionDenied {
                tool,
                reason: Some(reason),
                ..
            } => {
                let call = calls
                    .iter()
                    .rev()
                    .find(|c| c.name == *tool && !ends.contains_key(&c.id));
                if let Some(call) = call {
                    blocked.insert(call.id.clone(), reason.clone());
                }
            }
            _ => {}
        }
    }
    // Only edits that went through count.
    let succeeded = |call: &Call| ends.get(&call.id) == Some(&false);
    let mut files: Vec<FileChange> = Vec::new();
    for call in calls.iter().filter(|c| succeeded(c)) {
        let (Some((added, removed)), Some(path)) = (call.lines, &call.path) else {
            continue;
        };
        match files.iter_mut().find(|f| &f.path == path) {
            Some(f) => {
                f.added += added;
                f.removed += removed;
            }
            None => files.push(FileChange {
                path: path.clone(),
                added,
                removed,
            }),
        }
    }
    let answers = match_asks(
        &calls
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str(), &c.input))
            .collect::<Vec<_>>(),
        asks,
    );
    let skip = calls.len().saturating_sub(STEPS_SHOWN);
    let steps = calls
        .into_iter()
        .zip(answers)
        .skip(skip)
        .map(|(call, answer)| {
            let reason = blocked.get(&call.id).map(|r| humanize_reason(r));
            let state = match (ends.get(&call.id), &reason) {
                (_, Some(_)) => StepState::Blocked,
                (None, None) => StepState::Running,
                (Some(false), None) => StepState::Done,
                (Some(true), None) => StepState::Failed,
            };
            // A stopped call never ran: it reads as what it would have done.
            let verb = if state == StepState::Blocked {
                super::activity::verb(&call.kind, &call.name)
            } else {
                verb(&call.kind, &call.name, state == StepState::Running)
            };
            Step {
                outside: outside_sandbox(&call.input),
                verb,
                state,
                lines: call.lines.filter(|_| state == StepState::Done),
                target: call.target,
                reason,
                raw: raw_name(&call.name),
                answer,
            }
        })
        .collect();
    RunSummary { steps, files }
}

/// The verb for a tool call, ongoing or finished.
pub(super) fn verb(kind: &ToolKind, name: &str, ongoing: bool) -> String {
    let (now, then) = match kind {
        ToolKind::Read => ("Reading", "Read"),
        ToolKind::Search => ("Searching", "Searched"),
        ToolKind::Edit => ("Editing", "Edited"),
        ToolKind::Create => ("Creating", "Created"),
        ToolKind::Run => ("Running", "Ran"),
        ToolKind::Other => return tool_label(name),
    };
    if ongoing { now } else { then }.to_string()
}

/// `name` when [`tool_label`] shows it another way, for a tooltip.
pub(super) fn raw_name(name: &str) -> Option<String> {
    (tool_label(name) != name).then(|| name.to_string())
}

/// `path` relative to the run's folder, if it is inside it.
pub(super) fn relative(path: &str, cwd: &str) -> String {
    if cwd.is_empty() {
        return path.to_string();
    }
    path.strip_prefix(cwd)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
        .to_string()
}

pub(super) fn shorten(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > TARGET_MAX || line.len() < text.len() {
        let cut: String = line.chars().take(TARGET_MAX).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

impl Workspace {
    /// The live card of a queued or working run.
    pub(super) fn render_live_card(
        &self,
        run: &StoredRun,
        summary: &RunSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = run.id;
        let queued = matches!(run.status, crew_core::event::RunStatus::Queued);
        let ms = now_ms() - run.started_at;
        let (word, token, time) = if queued {
            ("Queued", "text-tertiary", String::new())
        } else {
            ("Working", "accent-blue", elapsed(ms))
        };
        let mut header = div()
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .h(scale::row(36.))
            .px(ui_px(12.))
            .text_size(scale::text(12.))
            .when(!queued, |header| {
                header.child(spinner(
                    SharedString::from(format!("run-spinner-{id}")),
                    14.,
                    token,
                ))
            })
            .child(
                div()
                    .id(format!("run-state-{id}"))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color(token))
                    .aria_label(word)
                    .child(word)
                    .test_support(),
            );
        if !time.is_empty() {
            // It ticks once a second, so the last value is a second ago.
            let prev = elapsed(ms - 1000);
            header = header.child(
                rolling_text(format!("run-elapsed-{id}"), &prev, &time)
                    .font_family(code_family())
                    .text_color(color("text-secondary"))
                    .aria_label(time.clone())
                    .test_support(),
            );
        }
        if let Some(counts) = summary.counts() {
            header = header.child(counts_line(id, summary, counts));
        }
        header = header
            .child(div().flex_1())
            .child(
                Button::new(format!("run-watch-{id}"))
                    .ghost()
                    .small()
                    .label("Watch")
                    .disabled(true)
                    .tooltip("Coming soon."),
            )
            .child(
                Button::new(format!("run-stop-{id}"))
                    .outline()
                    .small()
                    .text_color(color("danger"))
                    .icon(IconName::Square)
                    .label("Stop")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.stop_run(id, window, cx);
                    })),
            );
        card(id, "run-card")
            .child(header)
            .when(!summary.steps.is_empty(), |card| {
                card.child(
                    div()
                        .flex()
                        .flex_col()
                        .px(ui_px(12.))
                        .pb(scale::row(8.))
                        .children(summary.steps.iter().enumerate().map(step_row)),
                )
            })
            .test_support()
    }

    /// The card of a finished run that changed files.
    pub(super) fn render_done_card(
        &self,
        run: RunId,
        summary: &RunSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let counts = summary.counts().unwrap_or_default();
        let thread = self.state().run(run).map(|r| r.thread);
        let can_review = thread.is_some_and(|t| self.can_review(t));
        card(run, "done-card")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .h(scale::row(36.))
                    .px(ui_px(12.))
                    .text_size(scale::text(12.))
                    .child(
                        Icon::new(IconName::CircleCheck)
                            .size(ui_px(14.))
                            .text_color(color("success")),
                    )
                    .child(
                        div()
                            .font_weight(weight(FontWeight::MEDIUM))
                            .text_color(color("success"))
                            .child("Done"),
                    )
                    .child(counts_line(run, summary, counts))
                    .child(div().flex_1())
                    .child(
                        Button::new(format!("review-{run}"))
                            .outline()
                            .small()
                            .label("Review changes")
                            .disabled(!can_review)
                            .when(!can_review, |b| b.tooltip(NOTHING_TO_REVIEW))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(thread) = thread {
                                    this.open_review(thread, window, cx);
                                }
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(ui_px(12.))
                    .pb(scale::row(8.))
                    .children(summary.files.iter().map(|f| {
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .h(scale::row(24.))
                            .font_family(code_family())
                            .text_size(scale::mono(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(color("text-secondary"))
                                    .child(f.path.clone()),
                            )
                            .child(plus_minus(f.added, f.removed))
                    })),
            )
            .test_support()
    }
}

fn card(run: RunId, kind: &str) -> Stateful<Div> {
    div()
        .id(format!("{kind}-{run}"))
        .flex()
        .flex_col()
        .mt(ui_px(8.))
        .max_w(ui_px(560.))
        .border_1()
        .border_color(color("border-100"))
        .rounded(ui_px(8.))
        .bg(color("bg-100"))
}

/// `· 2 files · +42 −3`, labelled for tests and screen readers.
fn counts_line(run: RunId, summary: &RunSummary, counts: String) -> impl IntoElement {
    let n = summary.files.len();
    div()
        .id(format!("run-counts-{run}"))
        .flex()
        .items_center()
        .gap(ui_px(4.))
        .text_color(color("text-secondary"))
        .aria_label(counts)
        .child(format!("· {n} {} ·", if n == 1 { "file" } else { "files" }))
        .child(plus_minus(summary.added(), summary.removed()))
        .test_support()
}

fn plus_minus(added: u32, removed: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .gap(ui_px(4.))
        .font_family(code_family())
        .child(
            div()
                .text_color(color("success"))
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(color("danger"))
                .child(format!("−{removed}")),
        )
}

pub(super) fn step_row((ix, step): (usize, &Step)) -> impl IntoElement {
    let icon = match step.state {
        StepState::Done => Icon::new(IconName::Check)
            .size(ui_px(12.))
            .text_color(color("success"))
            .into_any_element(),
        StepState::Running => {
            spinner(("run-step-spinner", ix), 12., "accent-blue").into_any_element()
        }
        StepState::Failed => Icon::new(IconName::CircleX)
            .size(ui_px(12.))
            .text_color(color("danger"))
            .into_any_element(),
        StepState::Blocked => Icon::new(IconName::Ban)
            .size(ui_px(12.))
            .text_color(color("danger"))
            .into_any_element(),
    };
    let mut text = match (&step.state, &step.reason) {
        (StepState::Blocked, Some(reason)) if !reason.is_empty() => {
            format!("{} · Blocked · {reason}", step.text())
        }
        (StepState::Blocked, _) => format!("{} · Blocked", step.text()),
        _ => step.text(),
    };
    if let Some(answer) = step.answer {
        text = format!("{text} · {}", answer.text());
    }
    if step.outside {
        text = format!("{text} · {OUTSIDE_TAG}");
    }
    div()
        .id(("run-step", ix))
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(24.))
        .text_size(scale::text(12.))
        .text_color(color("text-secondary"))
        .aria_label(text)
        .when_some(step.raw.clone(), |row, raw| {
            row.tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(raw.clone()).build(window, cx)
            })
        })
        .child(icon)
        // A short verb stays whole beside its target; a long tool name
        // with no target is cut inside the card.
        .child(
            div()
                .id(("run-step-verb", ix))
                .map(|verb| {
                    if step.target.trim().is_empty() {
                        verb.min_w_0().truncate()
                    } else {
                        verb.flex_none()
                    }
                })
                .child(step.verb.clone())
                .test_support(),
        )
        .when(!step.target.trim().is_empty(), |row| {
            row.child(
                div()
                    .min_w_0()
                    .truncate()
                    .px(ui_px(4.))
                    .rounded(ui_px(4.))
                    .bg(color("surface-300"))
                    .font_family(code_family())
                    .text_color(color("text-primary"))
                    .child(step.target.clone()),
            )
        })
        .when_some(step.lines, |row, (added, removed)| {
            row.child(plus_minus(added, removed))
        })
        .when_some(step.answer, |row, answer| {
            row.child(
                div()
                    .flex_none()
                    .text_size(scale::text(11.))
                    .text_color(color("text-tertiary"))
                    .child(answer.text()),
            )
        })
        .when(step.outside, |row| {
            row.child(
                div()
                    .flex_none()
                    .text_size(scale::text(11.))
                    .text_color(color("danger"))
                    .child(OUTSIDE_TAG),
            )
        })
        .when(step.state == StepState::Blocked, |row| {
            row.child(
                div()
                    .flex_none()
                    .text_color(color("danger"))
                    .child("Blocked"),
            )
            .when_some(
                step.reason.clone().filter(|r| !r.is_empty()),
                |row, reason| {
                    row.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(color("text-tertiary"))
                            .child(reason),
                    )
                },
            )
        })
        .test_support()
}

#[cfg(test)]
mod tests {
    use super::{STEPS_SHOWN, StepState, TARGET_MAX, shorten, summarize};
    use crate::model::LoggedEvent;
    use crew_core::event::{Event, ToolKind};
    use crew_core::ids::RunId;

    fn call(run: RunId, seq: u64, id: &str, name: &str, input: serde_json::Value) -> LoggedEvent {
        let target = ["file_path", "command"]
            .iter()
            .find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(String::from));
        LoggedEvent::new(
            run,
            seq,
            Event::ToolCall {
                id: id.into(),
                name: name.into(),
                kind: ToolKind::from_tool_name(name),
                target,
                input,
            },
        )
    }

    fn result(run: RunId, seq: u64, id: &str, is_error: bool) -> LoggedEvent {
        LoggedEvent::new(
            run,
            seq,
            Event::ToolResult {
                id: id.into(),
                is_error,
                output: String::new(),
            },
        )
    }

    #[test]
    fn steps_and_counts_come_from_tool_events() {
        let run = RunId::new();
        let other = RunId::new();
        let events = vec![
            LoggedEvent::new(
                run,
                1,
                Event::SessionStarted {
                    session_id: "s".into(),
                    model: "m".into(),
                    cwd: "/work/demo".into(),
                },
            ),
            call(
                run,
                2,
                "t1",
                "Edit",
                serde_json::json!({"file_path": "/work/demo/src/lib.rs", "old_string": "a\nb", "new_string": "c"}),
            ),
            result(run, 3, "t1", false),
            call(
                run,
                4,
                "t2",
                "Write",
                serde_json::json!({"file_path": "/work/demo/new.rs", "content": "1\n2\n3\n"}),
            ),
            result(run, 5, "t2", false),
            call(
                run,
                6,
                "t3",
                "Bash",
                serde_json::json!({"command": "cargo test"}),
            ),
            result(run, 7, "t3", true),
            call(
                run,
                8,
                "t4",
                "Read",
                serde_json::json!({"file_path": "/elsewhere/x"}),
            ),
            call(other, 1, "t9", "Bash", serde_json::json!({"command": "ls"})),
        ];
        let summary = summarize(&events, run);
        let texts: Vec<(String, StepState)> =
            summary.steps.iter().map(|s| (s.text(), s.state)).collect();
        assert_eq!(
            texts,
            [
                ("Edited src/lib.rs".to_string(), StepState::Done),
                ("Created new.rs".to_string(), StepState::Done),
                ("Ran cargo test".to_string(), StepState::Failed),
                ("Reading /elsewhere/x".to_string(), StepState::Running),
            ]
        );
        assert_eq!(summary.steps[0].lines, Some((1, 2)));
        assert_eq!(summary.counts().as_deref(), Some("2 files · +4 −2"));
    }

    #[test]
    fn a_call_outside_the_sandbox_is_tagged() {
        let run = RunId::new();
        let events = vec![
            call(
                run,
                1,
                "t1",
                "Bash",
                serde_json::json!({"command": "curl x", "dangerouslyDisableSandbox": true}),
            ),
            call(run, 2, "t2", "Bash", serde_json::json!({"command": "ls"})),
            call(
                run,
                3,
                "t3",
                "Bash",
                serde_json::json!({"command": "pwd", "dangerouslyDisableSandbox": false}),
            ),
        ];
        let outside: Vec<bool> = summarize(&events, run)
            .steps
            .iter()
            .map(|s| s.outside)
            .collect();
        assert_eq!(outside, [true, false, false]);
    }

    #[test]
    fn only_successful_edits_count() {
        let run = RunId::new();
        let edit = |seq, id: &str| {
            call(
                run,
                seq,
                id,
                "Edit",
                serde_json::json!({"file_path": "/work/demo/a.rs", "old_string": "a", "new_string": "b\nc"}),
            )
        };
        let events = vec![edit(1, "t1"), result(run, 2, "t1", true), edit(3, "t2")];
        let summary = summarize(&events, run);
        assert_eq!(summary.counts(), None, "failed or still running");
        assert_eq!(summary.steps[0].lines, None);
        let mut events = events;
        events.push(result(run, 4, "t2", false));
        let summary = summarize(&events, run);
        assert_eq!(summary.counts().as_deref(), Some("1 file · +2 −1"));
        assert_eq!(summary.steps[1].lines, Some((2, 1)));
    }

    #[test]
    fn only_the_latest_steps_are_kept() {
        let run = RunId::new();
        let events: Vec<LoggedEvent> = (0..10)
            .map(|i| {
                call(
                    run,
                    i,
                    &format!("t{i}"),
                    "Bash",
                    serde_json::json!({"command": format!("step {i}")}),
                )
            })
            .collect();
        let summary = summarize(&events, run);
        assert_eq!(summary.steps.len(), STEPS_SHOWN);
        assert_eq!(summary.steps[0].text(), "Running step 4");
        assert_eq!(summary.counts(), None);
    }

    #[test]
    fn a_step_without_a_target_is_just_its_name() {
        let run = RunId::new();
        let events = vec![
            LoggedEvent::new(
                run,
                1,
                Event::Raw {
                    line: r#"{"type":"system","subtype":"background_tasks_changed"}"#.into(),
                },
            ),
            call(run, 2, "t1", "ToolSearch", serde_json::json!({})),
        ];
        let summary = summarize(&events, run);
        assert_eq!(summary.steps.len(), 1, "system lines aren't steps");
        assert_eq!(summary.steps[0].target, "");
        assert_eq!(summary.steps[0].text(), "ToolSearch");
    }

    #[test]
    fn a_call_the_safety_check_stopped_is_a_blocked_step() {
        let run = RunId::new();
        let rm = serde_json::json!({"command": "rm -rf build"});
        let denied = LoggedEvent::new(
            run,
            2,
            Event::PermissionDenied {
                tool: "Bash".into(),
                input: rm.clone(),
                reason: Some("Destructive or unwanted actions".into()),
            },
        );
        let refused = LoggedEvent::new(
            run,
            3,
            Event::ToolResult {
                id: "t1".into(),
                is_error: true,
                output: "Permission for this action has been denied by the Claude Code auto mode classifier. Reason: [Destructive or unwanted actions]".into(),
            },
        );
        let events = vec![call(run, 1, "t1", "Bash", rm), denied, refused.clone()];
        for events in [events.clone(), vec![events[0].clone(), refused]] {
            let step = summarize(&events, run).steps.remove(0);
            assert_eq!(step.state, StepState::Blocked);
            assert_eq!(step.text(), "Run rm -rf build");
            assert_eq!(
                step.reason.as_deref(),
                Some("Destructive or unwanted actions")
            );
        }
        // Before its result comes, the denial alone says so.
        let step = summarize(&events[..2], run).steps.remove(0);
        assert_eq!(step.state, StepState::Blocked);
    }

    #[test]
    fn mcp_steps_read_as_server_and_tool() {
        let run = RunId::new();
        let name = "mcp__plugin_chrome-devtools-mcp_chrome-devtools__navigate_page";
        let events = vec![
            call(run, 1, "t1", name, serde_json::json!({"url": "x"})),
            call(
                run,
                2,
                "t2",
                "mcp__crew_run__handoff",
                serde_json::json!({}),
            ),
        ];
        let steps = summarize(&events, run).steps;
        assert_eq!(steps[0].text(), "chrome-devtools · navigate page");
        assert_eq!(steps[0].raw.as_deref(), Some(name));
        assert_eq!(steps[1].text(), "mcp__crew_run__handoff");
        assert_eq!(steps[1].raw, None);
    }

    #[test]
    fn answers_find_their_calls_by_id_then_input_then_tool() {
        use super::{Answer, DecidedAsk, match_asks};
        let (a, b, c) = (
            serde_json::json!({"command": "a"}),
            serde_json::json!({"command": "b"}),
            serde_json::json!({"url": "x"}),
        );
        let calls = [
            ("t1", "Bash", &a),
            ("t2", "Bash", &b),
            ("t3", "Bash", &a),
            ("t4", "WebFetch", &c),
        ];
        let ask = |id: Option<&str>, tool: &str, input: &serde_json::Value, answer| DecidedAsk {
            tool_use_id: id.map(String::from),
            tool: tool.into(),
            input: input.clone(),
            answer,
        };
        let asks = [
            ask(None, "Bash", &a, Answer::Allowed),
            ask(Some("t1"), "Bash", &a, Answer::Denied),
            ask(None, "WebFetch", &serde_json::json!({}), Answer::Allowed),
        ];
        assert_eq!(
            match_asks(&calls, &asks),
            [
                Some(Answer::Denied),
                None,
                Some(Answer::Allowed),
                Some(Answer::Allowed)
            ]
        );
    }

    #[test]
    fn long_targets_are_cut() {
        let long = "x".repeat(100);
        assert_eq!(shorten(&long).chars().count(), TARGET_MAX + 1);
        assert_eq!(shorten("a\nb"), "a…");
        assert_eq!(shorten("ls"), "ls");
    }
}
