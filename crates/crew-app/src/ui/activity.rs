//! The inspector's Activity tab: one row per thing a run did, in a
//! virtual list so long runs stay cheap to draw.

use std::collections::HashMap;

use crew_core::event::{Event, ToolKind};
use crew_core::ids::RunId;
use crew_core::store::short_input;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::run_card::{StepState, changed_lines, relative};
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::model::LoggedEvent;
use crate::theme::color;

/// Output lines a Run row shows until you ask for all of it.
pub const OUTPUT_LINES: usize = 12;
/// Diff lines an Edit row shows.
pub const DIFF_LINES: usize = 12;

/// One line of an edit's mini diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub added: bool,
    pub text: String,
}

/// A tool call and, once it came, its result.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRow {
    pub id: String,
    pub kind: ToolKind,
    /// `Read`, `Edit` and so on, or the tool's own name.
    pub verb: String,
    pub target: String,
    pub state: StepState,
    /// Lines added and removed by an edit or a new file.
    pub lines: Option<(u32, u32)>,
    /// What came back, such as `212 lines` or `2 matches`.
    pub detail: Option<String>,
    pub diff: Vec<DiffLine>,
    /// A command's output.
    pub output: Option<String>,
}

/// One row of the Activity tab.
#[derive(Debug, Clone, PartialEq)]
pub enum ActivityRow {
    Thinking(String),
    Said(String),
    Tool(ToolRow),
    Denied { tool: String, input: String },
    Error(String),
    Raw(String),
    Finished { ok: bool, summary: String },
}

/// The rows for `run`'s events, in order. A tool's result fills in the
/// row of its call.
pub fn activity_rows(events: &[LoggedEvent], run: RunId) -> Vec<ActivityRow> {
    let mut cwd = String::new();
    let mut rows: Vec<ActivityRow> = Vec::new();
    let mut calls: HashMap<String, usize> = HashMap::new();
    for logged in events.iter().filter(|e| e.run == run) {
        match &logged.event {
            Event::SessionStarted { cwd: dir, .. } => cwd = dir.clone(),
            Event::Thinking { text } => rows.push(ActivityRow::Thinking(text.clone())),
            Event::Message { text } => rows.push(ActivityRow::Said(text.clone())),
            Event::ToolCall {
                id,
                name,
                kind,
                target,
                input,
            } => {
                calls.insert(id.clone(), rows.len());
                let target = target.as_deref().map(|t| relative(t, &cwd));
                rows.push(ActivityRow::Tool(ToolRow {
                    id: id.clone(),
                    kind: kind.clone(),
                    verb: verb(kind, name).to_string(),
                    target: target.unwrap_or_else(|| short_input(input)),
                    state: StepState::Running,
                    lines: changed_lines(kind, input),
                    detail: None,
                    diff: mini_diff(kind, input),
                    output: None,
                }));
            }
            Event::ToolResult {
                id,
                is_error,
                output,
            } => {
                if let Some(ActivityRow::Tool(row)) = calls.get(id).and_then(|ix| rows.get_mut(*ix))
                {
                    row.state = if *is_error {
                        StepState::Failed
                    } else {
                        StepState::Done
                    };
                    row.detail = detail(&row.kind, output, *is_error);
                    if row.kind == ToolKind::Run || *is_error {
                        row.output = Some(output.clone());
                    }
                }
            }
            Event::PermissionDenied { tool, input } => rows.push(ActivityRow::Denied {
                tool: tool.clone(),
                input: short_input(input),
            }),
            Event::Error { message } => rows.push(ActivityRow::Error(message.clone())),
            Event::Raw { line } => rows.push(ActivityRow::Raw(line.clone())),
            Event::TurnEnd { ok, summary, .. } => rows.push(ActivityRow::Finished {
                ok: *ok,
                summary: summary.clone(),
            }),
        }
    }
    rows
}

fn verb<'a>(kind: &ToolKind, name: &'a str) -> &'a str {
    match kind {
        ToolKind::Read => "Read",
        ToolKind::Search => "Search",
        ToolKind::Edit => "Edit",
        ToolKind::Create => "Create",
        ToolKind::Run => "Run",
        ToolKind::Other => name,
    }
}

/// What a finished call reports beside its target.
fn detail(kind: &ToolKind, output: &str, is_error: bool) -> Option<String> {
    if is_error {
        return Some("failed".into());
    }
    let n = output.lines().filter(|l| !l.trim().is_empty()).count();
    match kind {
        ToolKind::Read => Some(plural(n, "line", "lines")),
        ToolKind::Search => Some(plural(n, "match", "matches")),
        _ => None,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// An edit's removed lines, then its added ones, for each change.
fn mini_diff(kind: &ToolKind, input: &serde_json::Value) -> Vec<DiffLine> {
    if *kind != ToolKind::Edit {
        return Vec::new();
    }
    let edits: Vec<&serde_json::Value> = match input.get("edits").and_then(|e| e.as_array()) {
        Some(edits) => edits.iter().collect(),
        None => vec![input],
    };
    let mut lines = Vec::new();
    for edit in edits {
        for (key, added) in [("old_string", false), ("new_string", true)] {
            let text = edit.get(key).and_then(|v| v.as_str()).unwrap_or_default();
            lines.extend(text.lines().map(|l| DiffLine {
                added,
                text: l.to_string(),
            }));
        }
    }
    lines
}

/// The first `n` lines of `text` and how many more there are.
pub fn first_lines(text: &str, n: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.iter().take(n).copied().collect::<Vec<_>>().join("\n");
    (shown, lines.len().saturating_sub(n))
}

impl Workspace {
    /// Row `ix` of the Activity list.
    pub(super) fn render_activity_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.inspector.rows.get(ix) else {
            return div().into_any_element();
        };
        let (icon, token, content): (IconName, &str, AnyElement) = match row {
            ActivityRow::Thinking(text) => (
                IconName::Bot,
                "text-tertiary",
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_color(color("text-secondary")).child("Thinking"))
                    .child(
                        div()
                            .italic()
                            .text_color(color("text-tertiary"))
                            .child(text.clone()),
                    )
                    .into_any_element(),
            ),
            ActivityRow::Said(text) => (
                IconName::Bot,
                "text-secondary",
                div()
                    .text_color(color("text-primary"))
                    .child(text.clone())
                    .into_any_element(),
            ),
            ActivityRow::Tool(tool) => {
                let (icon, token) = tool_icon(tool);
                (
                    icon,
                    token,
                    self.render_tool(ix, tool, cx).into_any_element(),
                )
            }
            ActivityRow::Denied { tool, input } => (
                IconName::Ban,
                "danger",
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_color(color("text-secondary"))
                            .child(format!("Not allowed: {tool}")),
                    )
                    .child(code(input.clone()))
                    .into_any_element(),
            ),
            ActivityRow::Error(message) => (
                IconName::CircleX,
                "danger",
                div()
                    .text_color(color("danger"))
                    .child(message.clone())
                    .into_any_element(),
            ),
            ActivityRow::Raw(line) => (
                IconName::Ellipsis,
                "text-tertiary",
                div()
                    .font_family(mono_font())
                    .text_size(px(11.5))
                    .text_color(color("text-tertiary"))
                    .child(line.clone())
                    .into_any_element(),
            ),
            ActivityRow::Finished { ok, summary } => (
                if *ok {
                    IconName::CircleCheck
                } else {
                    IconName::CircleX
                },
                if *ok { "success" } else { "danger" },
                div()
                    .text_color(color("text-secondary"))
                    .child(if summary.is_empty() {
                        (if *ok { "Finished" } else { "Failed" }).to_string()
                    } else {
                        summary.clone()
                    })
                    .into_any_element(),
            ),
        };
        div()
            .id(("activity-row", ix))
            // Rows and their hairlines span the whole column.
            .w_full()
            .flex()
            .gap(px(8.))
            .px(px(16.))
            .py(px(10.))
            .border_b_1()
            .border_color(color("surface-200"))
            .text_size(px(12.))
            .line_height(px(18.))
            .aria_label(row_label(row))
            .child(
                div()
                    .flex_none()
                    .w(px(16.))
                    .pt(px(2.))
                    .child(Icon::new(icon).size(px(14.)).text_color(color(token))),
            )
            .child(div().flex_1().min_w_0().child(content))
            .test_support()
            .into_any_element()
    }

    fn render_tool(&self, ix: usize, tool: &ToolRow, cx: &mut Context<Self>) -> impl IntoElement {
        let mut head = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .text_color(color("text-secondary"))
                    .child(tool.verb.clone()),
            )
            .child(code(tool.target.clone()));
        if let (Some((added, removed)), StepState::Done) = (tool.lines, tool.state) {
            head = head
                .child(
                    div()
                        .font_family(mono_font())
                        .text_color(color("success"))
                        .child(format!("+{added}")),
                )
                .when(removed > 0, |head| {
                    head.child(
                        div()
                            .font_family(mono_font())
                            .text_color(color("danger"))
                            .child(format!("−{removed}")),
                    )
                });
        }
        if tool.state == StepState::Running {
            head = head.child(div().text_color(color("accent-blue")).child("running"));
        } else if let Some(detail) = &tool.detail {
            let token = if tool.state == StepState::Failed {
                "danger"
            } else {
                "text-tertiary"
            };
            head = head.child(div().text_color(color(token)).child(detail.clone()));
        }
        let diff = (!tool.diff.is_empty()).then(|| {
            let more = tool.diff.len().saturating_sub(DIFF_LINES);
            div()
                .mt(px(6.))
                .flex()
                .flex_col()
                .border_1()
                .border_color(color("border-100"))
                .rounded(px(6.))
                .overflow_hidden()
                .font_family(mono_font())
                .text_size(px(11.5))
                .children(tool.diff.iter().take(DIFF_LINES).map(|line| {
                    let (sign, bg) = if line.added {
                        ("+", "success-subtle")
                    } else {
                        ("−", "danger-subtle")
                    };
                    div()
                        .flex()
                        .gap(px(8.))
                        .px(px(8.))
                        .bg(color(bg))
                        .child(
                            div()
                                .flex_none()
                                .text_color(color("text-tertiary"))
                                .child(sign),
                        )
                        .child(div().min_w_0().truncate().child(line.text.clone()))
                }))
                .when(more > 0, |diff| {
                    diff.child(
                        div()
                            .px(px(8.))
                            .text_color(color("text-tertiary"))
                            .child(format!("{more} more lines")),
                    )
                })
        });
        let output = tool.output.as_ref().filter(|o| !o.is_empty()).map(|text| {
            let key = self
                .inspector
                .rows_from
                .map(|(run, _)| (run, tool.id.clone()));
            let expanded = key
                .as_ref()
                .is_some_and(|k| self.inspector.expanded.contains(k));
            let (shown, more) = if expanded {
                (text.clone(), 0)
            } else {
                first_lines(text, OUTPUT_LINES)
            };
            div()
                .mt(px(6.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .id(("activity-output", ix))
                        .px(px(8.))
                        .py(px(6.))
                        .bg(color("bg-100"))
                        .border_1()
                        .border_color(color("border-100"))
                        .rounded(px(6.))
                        .font_family(mono_font())
                        .text_size(px(11.5))
                        .text_color(color("text-secondary"))
                        .aria_label(shown.clone())
                        .child(shown)
                        .test_support(),
                )
                .when(more > 0, |block| {
                    let key = key.clone();
                    block.child(
                        div()
                            .id(("activity-show-all", ix))
                            .text_color(color("accent-blue"))
                            .cursor_pointer()
                            .aria_label(format!("Show all ({more} more lines)"))
                            .child(format!("Show all ({more} more lines)"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(key) = key.clone() {
                                    this.inspector.expanded.insert(key);
                                }
                                this.inspector.list.remeasure_items(ix..ix + 1);
                                cx.notify();
                            }))
                            .test_support(),
                    )
                })
        });
        div()
            .flex()
            .flex_col()
            .child(head)
            .children(diff)
            .children(output)
    }
}

fn tool_icon(tool: &ToolRow) -> (IconName, &'static str) {
    if tool.state == StepState::Failed {
        return (IconName::CircleX, "danger");
    }
    let icon = match tool.kind {
        ToolKind::Read => IconName::FileText,
        ToolKind::Search => IconName::Search,
        ToolKind::Edit => IconName::Replace,
        ToolKind::Create => IconName::Plus,
        ToolKind::Run => IconName::SquareTerminal,
        ToolKind::Other => IconName::Ellipsis,
    };
    let token = if tool.state == StepState::Running {
        "accent-blue"
    } else {
        "text-secondary"
    };
    (icon, token)
}

/// What a row says, for tests and screen readers.
pub fn row_label(row: &ActivityRow) -> String {
    match row {
        ActivityRow::Thinking(text) => format!("Thinking {text}"),
        ActivityRow::Said(text) => format!("Said {text}"),
        ActivityRow::Tool(tool) => format!("{} {}", tool.verb, tool.target),
        ActivityRow::Denied { tool, input } => format!("Not allowed: {tool} {input}"),
        ActivityRow::Error(message) => message.clone(),
        ActivityRow::Raw(line) => line.clone(),
        ActivityRow::Finished { ok: true, summary } => format!("Finished {summary}"),
        ActivityRow::Finished { ok: false, summary } => format!("Failed {summary}"),
    }
}

/// Mono text on a chip, for paths and commands.
fn code(text: String) -> Div {
    div()
        .min_w_0()
        .truncate()
        .px(px(4.))
        .rounded(px(4.))
        .bg(color("surface-300"))
        .font_family(mono_font())
        .text_color(color("text-primary"))
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::{ActivityRow, DiffLine, activity_rows, first_lines};
    use crate::model::LoggedEvent;
    use crate::ui::run_card::StepState;
    use crew_core::event::{Event, ToolKind};
    use crew_core::ids::RunId;

    fn at(run: RunId, seq: u64, event: Event) -> LoggedEvent {
        LoggedEvent { run, seq, event }
    }

    fn call(name: &str, id: &str, target: Option<&str>, input: serde_json::Value) -> Event {
        Event::ToolCall {
            id: id.into(),
            name: name.into(),
            kind: ToolKind::from_tool_name(name),
            target: target.map(String::from),
            input,
        }
    }

    fn result(id: &str, is_error: bool, output: &str) -> Event {
        Event::ToolResult {
            id: id.into(),
            is_error,
            output: output.into(),
        }
    }

    #[test]
    fn rows_follow_the_runs_events() {
        let run = RunId::new();
        let other = RunId::new();
        let events = vec![
            at(
                run,
                0,
                Event::SessionStarted {
                    session_id: "s".into(),
                    model: "m".into(),
                    cwd: "/work/demo".into(),
                },
            ),
            at(run, 1, Event::Thinking { text: "hmm".into() }),
            at(
                run,
                2,
                call(
                    "Read",
                    "t1",
                    Some("/work/demo/a.rs"),
                    serde_json::json!({"file_path": "/work/demo/a.rs"}),
                ),
            ),
            at(run, 3, result("t1", false, "1\n2\n3")),
            at(
                run,
                4,
                call(
                    "Edit",
                    "t2",
                    Some("/work/demo/a.rs"),
                    serde_json::json!({"file_path": "/work/demo/a.rs", "old_string": "a", "new_string": "b\nc"}),
                ),
            ),
            at(run, 5, result("t2", false, "ok")),
            at(
                run,
                6,
                call(
                    "Bash",
                    "t3",
                    Some("cargo test"),
                    serde_json::json!({"command": "cargo test"}),
                ),
            ),
            at(
                other,
                1,
                Event::Raw {
                    line: "elsewhere".into(),
                },
            ),
            at(
                run,
                7,
                Event::Raw {
                    line: "not json".into(),
                },
            ),
            at(
                run,
                8,
                Event::Error {
                    message: "boom".into(),
                },
            ),
            at(
                run,
                9,
                Event::PermissionDenied {
                    tool: "Bash".into(),
                    input: serde_json::json!({"command": "rm -rf x"}),
                },
            ),
        ];
        let rows = activity_rows(&events, run);
        assert_eq!(rows.len(), 7, "{rows:?}");
        assert_eq!(rows[0], ActivityRow::Thinking("hmm".into()));
        let ActivityRow::Tool(read) = &rows[1] else {
            panic!("a read row")
        };
        assert_eq!((read.verb.as_str(), read.target.as_str()), ("Read", "a.rs"));
        assert_eq!(read.detail.as_deref(), Some("3 lines"));
        let ActivityRow::Tool(edit) = &rows[2] else {
            panic!("an edit row")
        };
        assert_eq!(edit.lines, Some((2, 1)));
        assert_eq!(
            edit.diff,
            [
                DiffLine {
                    added: false,
                    text: "a".into()
                },
                DiffLine {
                    added: true,
                    text: "b".into()
                },
                DiffLine {
                    added: true,
                    text: "c".into()
                },
            ]
        );
        let ActivityRow::Tool(bash) = &rows[3] else {
            panic!("a run row")
        };
        assert_eq!(bash.state, StepState::Running);
        assert_eq!(bash.output, None);
        assert_eq!(rows[4], ActivityRow::Raw("not json".into()));
        assert_eq!(rows[5], ActivityRow::Error("boom".into()));
        assert_eq!(
            rows[6],
            ActivityRow::Denied {
                tool: "Bash".into(),
                input: "rm -rf x".into()
            }
        );

        // The result fills in the run row in place.
        let mut events = events;
        events.push(at(run, 10, result("t3", true, "test failed")));
        let rows = activity_rows(&events, run);
        let ActivityRow::Tool(bash) = &rows[3] else {
            panic!("a run row")
        };
        assert_eq!(bash.state, StepState::Failed);
        assert_eq!(bash.output.as_deref(), Some("test failed"));
    }

    #[test]
    fn long_output_shows_its_first_lines() {
        let text: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let (shown, more) = first_lines(&text.join("\n"), 12);
        assert_eq!(shown.lines().count(), 12);
        assert_eq!(more, 8);
        assert_eq!(first_lines("one", 12), ("one".to_string(), 0));
    }
}
