//! The card under a run's message: its last steps, time and file counts
//! while it works, and what it changed once it is done.

use std::collections::HashMap;

use crew_core::event::{Event, ToolKind};
use crew_core::ids::RunId;
use crew_core::store::StoredRun;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{elapsed, now_ms};
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::model::LoggedEvent;
use crate::theme::color;

/// How many of the latest steps a card lists.
const STEPS_SHOWN: usize = 6;
/// Longest step target before it is cut short.
const TARGET_MAX: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Done,
    Running,
    Failed,
}

/// One tool call of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub state: StepState,
    pub verb: String,
    pub target: String,
    /// Lines added and removed, for edits and new files.
    pub lines: Option<(u32, u32)>,
}

impl Step {
    /// What the step reads as, such as `Edited src/lib.rs`.
    pub fn text(&self) -> String {
        format!("{} {}", self.verb, self.target)
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
    kind: ToolKind,
    name: String,
    /// Relative to the run's folder.
    path: Option<String>,
    target: String,
    lines: Option<(u32, u32)>,
}

/// Sums up `run`'s events among a thread's.
pub fn summarize(events: &[LoggedEvent], run: RunId) -> RunSummary {
    let mut cwd = String::new();
    let mut calls: Vec<Call> = Vec::new();
    let mut ends: HashMap<String, bool> = HashMap::new();
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
                    kind: kind.clone(),
                    name: name.clone(),
                    target: shorten(target.as_deref().unwrap_or_default()),
                    path: target,
                    lines: changed_lines(kind, input),
                });
            }
            Event::ToolResult { id, is_error, .. } => {
                ends.insert(id.clone(), *is_error);
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
    let skip = calls.len().saturating_sub(STEPS_SHOWN);
    let steps = calls
        .into_iter()
        .skip(skip)
        .map(|call| {
            let state = match ends.get(&call.id) {
                None => StepState::Running,
                Some(false) => StepState::Done,
                Some(true) => StepState::Failed,
            };
            Step {
                verb: verb(&call.kind, &call.name, state == StepState::Running),
                state,
                lines: call.lines.filter(|_| state == StepState::Done),
                target: call.target,
            }
        })
        .collect();
    RunSummary { steps, files }
}

/// The verb for a tool call, ongoing or finished.
fn verb(kind: &ToolKind, name: &str, ongoing: bool) -> String {
    let (now, then) = match kind {
        ToolKind::Read => ("Reading", "Read"),
        ToolKind::Search => ("Searching", "Searched"),
        ToolKind::Edit => ("Editing", "Edited"),
        ToolKind::Create => ("Creating", "Created"),
        ToolKind::Run => ("Running", "Ran"),
        ToolKind::Other => return name.to_string(),
    };
    if ongoing { now } else { then }.to_string()
}

/// Lines an edit or a new file adds and removes.
fn changed_lines(kind: &ToolKind, input: &serde_json::Value) -> Option<(u32, u32)> {
    let lines = |v: &serde_json::Value| {
        v.as_str()
            .map_or(0, |s| u32::try_from(s.lines().count()).unwrap_or(u32::MAX))
    };
    match kind {
        ToolKind::Edit => {
            let edits = match input.get("edits").and_then(|e| e.as_array()) {
                Some(edits) => edits.iter().collect(),
                None => vec![input],
            };
            Some(edits.iter().fold((0, 0), |(a, r), e| {
                (a + lines(&e["new_string"]), r + lines(&e["old_string"]))
            }))
        }
        ToolKind::Create => Some((lines(&input["content"]), 0)),
        _ => None,
    }
}

/// `path` relative to the run's folder, if it is inside it.
fn relative(path: &str, cwd: &str) -> String {
    if cwd.is_empty() {
        return path.to_string();
    }
    path.strip_prefix(cwd)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
        .to_string()
}

fn shorten(text: &str) -> String {
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
        let (word, token, time) = if queued {
            ("Queued", "text-tertiary", String::new())
        } else {
            ("Working", "accent-blue", elapsed(now_ms() - run.started_at))
        };
        let mut header = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(36.))
            .px(px(12.))
            .text_size(px(12.))
            .when(!queued, |header| {
                header.child(
                    Icon::new(IconName::LoaderCircle)
                        .size(px(14.))
                        .text_color(color(token)),
                )
            })
            .child(
                div()
                    .id(format!("run-state-{id}"))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color(token))
                    .aria_label(word)
                    .child(word)
                    .test_support(),
            );
        if !time.is_empty() {
            header = header.child(
                div()
                    .id(format!("run-elapsed-{id}"))
                    .font_family(mono_font())
                    .text_color(color("text-secondary"))
                    .aria_label(time.clone())
                    .child(time)
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
                        .px(px(12.))
                        .pb(px(8.))
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
    ) -> impl IntoElement + use<> {
        let counts = summary.counts().unwrap_or_default();
        card(run, "done-card")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(36.))
                    .px(px(12.))
                    .text_size(px(12.))
                    .child(
                        Icon::new(IconName::CircleCheck)
                            .size(px(14.))
                            .text_color(color("success")),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
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
                            .disabled(true)
                            .tooltip("Coming soon."),
                    ),
            )
            .child(div().flex().flex_col().px(px(12.)).pb(px(8.)).children(
                summary.files.iter().map(|f| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .h(px(24.))
                        .font_family(mono_font())
                        .text_size(px(12.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(color("text-secondary"))
                                .child(f.path.clone()),
                        )
                        .child(plus_minus(f.added, f.removed))
                }),
            ))
            .test_support()
    }
}

fn card(run: RunId, kind: &str) -> Stateful<Div> {
    div()
        .id(format!("{kind}-{run}"))
        .flex()
        .flex_col()
        .mt(px(8.))
        .max_w(px(560.))
        .border_1()
        .border_color(color("border-100"))
        .rounded(px(8.))
        .bg(color("bg-100"))
}

/// `· 2 files · +42 −3`, labelled for tests and screen readers.
fn counts_line(run: RunId, summary: &RunSummary, counts: String) -> impl IntoElement {
    let n = summary.files.len();
    div()
        .id(format!("run-counts-{run}"))
        .flex()
        .items_center()
        .gap(px(4.))
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
        .gap(px(4.))
        .font_family(mono_font())
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

fn step_row((ix, step): (usize, &Step)) -> impl IntoElement {
    let (icon, token) = match step.state {
        StepState::Done => (IconName::Check, "success"),
        StepState::Running => (IconName::LoaderCircle, "accent-blue"),
        StepState::Failed => (IconName::CircleX, "danger"),
    };
    let text = step.text();
    div()
        .id(("run-step", ix))
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(24.))
        .text_size(px(12.))
        .text_color(color("text-secondary"))
        .aria_label(text)
        .child(Icon::new(icon).size(px(12.)).text_color(color(token)))
        .child(step.verb.clone())
        .child(
            div()
                .min_w_0()
                .truncate()
                .px(px(4.))
                .rounded(px(4.))
                .bg(color("surface-300"))
                .font_family(mono_font())
                .text_color(color("text-primary"))
                .child(step.target.clone()),
        )
        .when_some(step.lines, |row, (added, removed)| {
            row.child(plus_minus(added, removed))
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
        LoggedEvent {
            run,
            seq,
            event: Event::ToolCall {
                id: id.into(),
                name: name.into(),
                kind: ToolKind::from_tool_name(name),
                target,
                input,
            },
        }
    }

    fn result(run: RunId, seq: u64, id: &str, is_error: bool) -> LoggedEvent {
        LoggedEvent {
            run,
            seq,
            event: Event::ToolResult {
                id: id.into(),
                is_error,
                output: String::new(),
            },
        }
    }

    #[test]
    fn steps_and_counts_come_from_tool_events() {
        let run = RunId::new();
        let other = RunId::new();
        let events = vec![
            LoggedEvent {
                run,
                seq: 1,
                event: Event::SessionStarted {
                    session_id: "s".into(),
                    model: "m".into(),
                    cwd: "/work/demo".into(),
                },
            },
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
    fn long_targets_are_cut() {
        let long = "x".repeat(100);
        assert_eq!(shorten(&long).chars().count(), TARGET_MAX + 1);
        assert_eq!(shorten("a\nb"), "a…");
        assert_eq!(shorten("ls"), "ls");
    }
}
