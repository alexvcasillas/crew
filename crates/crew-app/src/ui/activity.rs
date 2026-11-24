//! The inspector's Activity tab: one row per thing a run did, in a
//! virtual list so long runs stay cheap to draw.

use std::collections::HashMap;

use crew_core::event::{Event, ToolKind};
use crew_core::ids::RunId;
use crew_core::store::short_input;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{clean_line, spinner};
use super::markdown_view::{DocKey, render_doc};
use super::run_card::{StepState, relative};
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::model::{LoggedEvent, PatchMark, patch_marks};
use crate::theme::color;

/// Output lines a Run row shows until you ask for all of it.
pub const OUTPUT_LINES: usize = 12;
/// Diff lines an Edit row shows.
pub const DIFF_LINES: usize = 12;
/// Lines of a unified diff an Edit row shows until you ask for all of it.
pub const PATCH_LINES: usize = 20;
/// How tall a diff shown in full grows before it scrolls.
const PATCH_MAX_HEIGHT: f32 = 480.;

/// One line of an edit's mini diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub added: bool,
    pub text: String,
}

/// One line of a unified diff, cleaned, with its tint if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchLine {
    pub text: String,
    pub tint: Option<&'static str>,
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
    /// A unified diff, as Codex gives an edit in place of its old and new
    /// text.
    pub patch: Option<Vec<PatchLine>>,
    /// A command's output.
    pub output: Option<String>,
}

/// One row of the Activity tab.
#[derive(Debug, Clone, PartialEq)]
pub enum ActivityRow {
    Thinking(String),
    /// What the agent said, with its event's number in the run.
    Said {
        seq: u64,
        text: String,
    },
    Tool(ToolRow),
    Denied {
        tool: String,
        input: String,
    },
    Error(String),
    /// The run's end, with its event's number and the agent's summary.
    Finished {
        ok: bool,
        seq: u64,
        summary: String,
    },
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
            // Empty thinking says nothing.
            Event::Thinking { text } if text.trim().is_empty() => {}
            Event::Thinking { text } => rows.push(ActivityRow::Thinking(text.clone())),
            Event::Message { text } => rows.push(ActivityRow::Said {
                seq: logged.seq,
                text: text.clone(),
            }),
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
                    lines: logged.lines,
                    detail: None,
                    diff: mini_diff(kind, input),
                    patch: patch(kind, input),
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
            // Lines the parser didn't know, such as stream-json system
            // notes, stay in the store but aren't shown.
            Event::Raw { .. } => {}
            Event::TurnEnd { ok, summary, .. } => rows.push(ActivityRow::Finished {
                ok: *ok,
                seq: logged.seq,
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

/// An edit's unified diff, if it came as one, cleaned and tinted once
/// here rather than on every redraw.
fn patch(kind: &ToolKind, input: &serde_json::Value) -> Option<Vec<PatchLine>> {
    if *kind != ToolKind::Edit {
        return None;
    }
    let diff = input
        .get("diff")
        .and_then(|d| d.as_str())
        .filter(|d| !d.trim().is_empty())?;
    Some(
        patch_marks(diff)
            .map(|(line, mark)| PatchLine {
                text: clean_line(line),
                tint: match mark {
                    PatchMark::Added => Some("success-subtle"),
                    PatchMark::Removed => Some("danger-subtle"),
                    PatchMark::Plain => None,
                },
            })
            .collect(),
    )
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
            ActivityRow::Said { seq, text } => (
                IconName::Bot,
                "text-secondary",
                self.render_said(ix, *seq, text),
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
                    .when(!input.trim().is_empty(), |row| {
                        row.child(code(input.clone()))
                    })
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
            ActivityRow::Finished { ok, seq, summary } => (
                if *ok {
                    IconName::CircleCheck
                } else {
                    IconName::CircleX
                },
                if *ok { "success" } else { "danger" },
                if summary.is_empty() {
                    div()
                        .text_color(color("text-secondary"))
                        .child(if *ok { "Finished" } else { "Failed" })
                        .into_any_element()
                } else {
                    self.render_said(ix, *seq, summary)
                },
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
            .child(div().flex_none().w(px(16.)).pt(px(2.)).child(
                if matches!(row, ActivityRow::Tool(t) if t.state == StepState::Running) {
                    spinner(("activity-spinner", ix), 14., token).into_any_element()
                } else {
                    Icon::new(icon)
                        .size(px(14.))
                        .text_color(color(token))
                        .into_any_element()
                },
            ))
            .child(div().flex_1().min_w_0().child(content))
            .test_support()
            .into_any_element()
    }

    /// Text an agent wrote, as Markdown parsed once per event.
    fn render_said(&self, ix: usize, seq: u64, text: &str) -> AnyElement {
        let Some((run, _)) = self.inspector.rows_from else {
            return div().child(text.to_string()).into_any_element();
        };
        let doc = self.docs.get(DocKey::Event(run, seq), text);
        let names: Vec<String> = self
            .state()
            .agents()
            .iter()
            .map(|a| a.spec.name.clone())
            .collect();
        render_doc(&doc, &names, &format!("md-activity-{ix}")).into_any_element()
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
            .when(!tool.target.trim().is_empty(), |head| {
                head.child(code(tool.target.clone()))
            });
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
        let patch = tool.patch.as_ref().map(|lines| {
            self.render_patch(ix, &tool.id, lines, cx)
                .into_any_element()
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
            .children(patch)
            .children(output)
    }

    /// A unified diff: its first [`PATCH_LINES`], added lines green and
    /// removed ones red, and "Show all" for the rest, which scrolls past
    /// [`PATCH_MAX_HEIGHT`].
    fn render_patch(
        &self,
        ix: usize,
        tool_id: &str,
        lines: &[PatchLine],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = self
            .inspector
            .rows_from
            .map(|(run, _)| (run, format!("{tool_id}#diff")));
        let expanded = key
            .as_ref()
            .is_some_and(|k| self.inspector.expanded.contains(k));
        let shown = if expanded {
            lines
        } else {
            &lines[..lines.len().min(PATCH_LINES)]
        };
        let more = lines.len() - shown.len();
        let label = shown
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        div()
            .mt(px(6.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .id(("activity-patch", ix))
                    .flex()
                    .flex_col()
                    .py(px(2.))
                    .max_h(px(PATCH_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(color("border-100"))
                    .rounded(px(6.))
                    .font_family(mono_font())
                    .text_size(px(11.5))
                    .aria_label(label)
                    .children(shown.iter().map(|line| {
                        div()
                            .flex_none()
                            .px(px(8.))
                            .min_w_0()
                            .truncate()
                            .when_some(line.tint, |row, tint| row.bg(color(tint)))
                            .when(line.text.starts_with("@@"), |row| {
                                row.text_color(color("text-tertiary"))
                            })
                            .child(line.text.clone())
                    }))
                    .test_support(),
            )
            .when(more > 0, |block| {
                block.child(
                    div()
                        .id(("activity-patch-all", ix))
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
        ActivityRow::Said { text, .. } => format!("Said {text}"),
        ActivityRow::Tool(tool) => join_words(&tool.verb, &tool.target),
        ActivityRow::Denied { tool, input } => join_words(&format!("Not allowed: {tool}"), input),
        ActivityRow::Error(message) => message.clone(),
        ActivityRow::Finished {
            ok: true, summary, ..
        } => format!("Finished {summary}"),
        ActivityRow::Finished {
            ok: false, summary, ..
        } => format!("Failed {summary}"),
    }
}

/// `head rest`, or just `head` when there is no rest.
fn join_words(head: &str, rest: &str) -> String {
    if rest.trim().is_empty() {
        head.to_string()
    } else {
        format!("{head} {rest}")
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
        LoggedEvent::new(run, seq, event)
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
        assert_eq!(rows.len(), 6, "raw lines aren't rows: {rows:?}");
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
        assert_eq!(rows[4], ActivityRow::Error("boom".into()));
        assert_eq!(
            rows[5],
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
    fn system_lines_and_empty_thinking_are_not_shown() {
        let run = RunId::new();
        let events = vec![
            at(
                run,
                1,
                Event::Raw {
                    line: r#"{"type":"system","subtype":"task_started"}"#.into(),
                },
            ),
            at(
                run,
                2,
                Event::Thinking {
                    text: "  \n".into(),
                },
            ),
            at(
                run,
                3,
                call("ToolSearch", "t1", None, serde_json::json!({})),
            ),
        ];
        let rows = activity_rows(&events, run);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let ActivityRow::Tool(tool) = &rows[0] else {
            panic!("a tool row")
        };
        assert_eq!(tool.verb, "ToolSearch");
        assert_eq!(super::row_label(&rows[0]), "ToolSearch");
        let denied = ActivityRow::Denied {
            tool: "Bash".into(),
            input: String::new(),
        };
        assert_eq!(super::row_label(&denied), "Not allowed: Bash");
    }

    #[test]
    fn codex_edits_keep_their_unified_diff() {
        let run = RunId::new();
        // A removed `-- comment` line inside a hunk is a change, not a
        // file header.
        let diff = "--- a/a.sql\n+++ b/a.sql\n@@ -1,2 +1,2 @@\n--- old note\n-a\n+b\n++c\n same\r\tend\u{202e}x\u{7}\n";
        let events = vec![
            at(
                run,
                0,
                call(
                    "Edit",
                    "exec-1",
                    Some("/work/demo/a.sql"),
                    serde_json::json!({"path": "/work/demo/a.sql", "diff": diff}),
                ),
            ),
            at(run, 1, result("exec-1", false, "Applied")),
        ];
        let rows = activity_rows(&events, run);
        let ActivityRow::Tool(edit) = &rows[0] else {
            panic!("an edit row")
        };
        assert!(edit.diff.is_empty(), "no old and new text");
        assert_eq!(edit.lines, Some((2, 2)));
        let patch = edit.patch.as_ref().unwrap();
        let tints: Vec<(&str, Option<&str>)> =
            patch.iter().map(|l| (l.text.as_str(), l.tint)).collect();
        assert_eq!(
            tints,
            [
                ("--- a/a.sql", None),
                ("+++ b/a.sql", None),
                ("@@ -1,2 +1,2 @@", None),
                ("--- old note", Some("danger-subtle")),
                ("-a", Some("danger-subtle")),
                ("+b", Some("success-subtle")),
                ("++c", Some("success-subtle")),
                // Control and bidi characters never reach the screen.
                (" same  endx", None),
            ]
        );
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
