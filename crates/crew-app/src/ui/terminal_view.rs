//! A Claude Code session run in a terminal, shown read-only: what you
//! typed and what Claude did, in a virtual list. There is no composer;
//! you reply in the terminal.

use std::collections::{HashMap, HashSet};

use crew_core::claude::transcript::TranscriptItem;
use crew_core::event::{Event, ToolKind};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{chip, clean_line, clean_lines, cut_start, monogram};
use super::markdown_view::{DocCache, DocKey, render_doc};
use super::run_card::{Step, StepState, relative, shorten, step_row, verb};
use super::workspace::Workspace;
use crate::model::changed_lines;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// What the open session's header shows of its folder.
const CWD_MAX: usize = 60;
/// The name over Claude's turns.
const AGENT: &str = "Claude";
/// The left edge of a turn's content, past the avatar.
const INDENT: f32 = 24. + 32. + 12.;
/// Most characters of one prompt, message or thought drawn.
pub const TEXT_MAX: usize = 20_000;

/// One row of a terminal session's list.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalRow {
    /// What you typed, with its seq.
    You {
        seq: u64,
        text: String,
    },
    /// Claude starts a turn.
    Agent,
    /// What Claude said, as Markdown.
    Said {
        seq: u64,
        text: String,
    },
    /// Claude's thinking, shown when you open it.
    Thinking {
        seq: u64,
        text: String,
    },
    /// Tool calls in a row, each filled in by its result.
    Steps(Vec<Step>),
    Error(String),
}

/// The rows for a session's items: a "You" row per prompt, and Claude's
/// output after it grouped like a run.
pub fn terminal_rows<'a>(
    items: impl IntoIterator<Item = (u64, &'a TranscriptItem)>,
    cwd: &str,
) -> Vec<TerminalRow> {
    let mut rows: Vec<TerminalRow> = Vec::new();
    let mut calls: HashMap<&str, Call> = HashMap::new();
    for (seq, item) in items {
        let event = match item {
            TranscriptItem::UserPrompt(text) => {
                rows.push(TerminalRow::You {
                    seq,
                    text: shown(text),
                });
                continue;
            }
            TranscriptItem::Event(event) => event,
        };
        let row = match event {
            Event::Message { text } if !text.trim().is_empty() => TerminalRow::Said {
                seq,
                text: shown(text),
            },
            Event::Thinking { text } if !text.trim().is_empty() => TerminalRow::Thinking {
                seq,
                text: shown(text),
            },
            Event::ToolCall {
                id,
                name,
                kind,
                target,
                input,
            } => {
                let target = target.as_deref().map(|t| relative(t, cwd));
                let step = Step {
                    state: StepState::Running,
                    verb: verb(kind, name, true),
                    target: clean_line(&shorten(target.as_deref().unwrap_or_default())),
                    lines: None,
                    reason: None,
                    raw: super::run_card::raw_name(name),
                    answer: None,
                    outside: super::approval_card::outside_sandbox(input),
                };
                start_turn(&mut rows);
                let last = rows.len() - 1;
                let (row, ix) = match rows.last_mut() {
                    Some(TerminalRow::Steps(steps)) => {
                        steps.push(step);
                        (last, steps.len() - 1)
                    }
                    _ => {
                        rows.push(TerminalRow::Steps(vec![step]));
                        (rows.len() - 1, 0)
                    }
                };
                let lines = changed_lines(kind, input);
                calls.insert(
                    id,
                    Call {
                        row,
                        ix,
                        kind,
                        name,
                        lines,
                    },
                );
                continue;
            }
            Event::ToolResult { id, is_error, .. } => {
                if let Some(call) = calls.get(id.as_str())
                    && let Some(TerminalRow::Steps(steps)) = rows.get_mut(call.row)
                    && let Some(step) = steps.get_mut(call.ix)
                {
                    finish(step, call, *is_error);
                }
                continue;
            }
            Event::Error { message } => TerminalRow::Error(shown(message)),
            Event::PermissionDenied { tool, .. } => {
                TerminalRow::Error(format!("Not allowed: {}", clean_line(tool)))
            }
            _ => continue,
        };
        start_turn(&mut rows);
        rows.push(row);
    }
    rows
}

/// Text as drawn: cleaned, and cut to [`TEXT_MAX`] characters with `…`.
fn shown(text: &str) -> String {
    let text = clean_lines(text);
    match text.char_indices().nth(TEXT_MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    }
}

/// Puts Claude's name over its output, once per turn.
fn start_turn(rows: &mut Vec<TerminalRow>) {
    if matches!(rows.last(), None | Some(TerminalRow::You { .. })) {
        rows.push(TerminalRow::Agent);
    }
}

/// A tool call as first seen: where its step is, and what finishing it
/// needs.
struct Call<'a> {
    row: usize,
    ix: usize,
    kind: &'a ToolKind,
    name: &'a str,
    lines: Option<(u32, u32)>,
}

/// A finished step: past tense, and its line counts if it worked, as the
/// run card shows it.
fn finish(step: &mut Step, call: &Call, is_error: bool) {
    step.state = if is_error {
        StepState::Failed
    } else {
        StepState::Done
    };
    step.verb = verb(call.kind, call.name, false);
    step.lines = call.lines.filter(|_| !is_error);
}

/// The open terminal session and what its list keeps between frames.
pub(super) struct TerminalView {
    pub(super) session: String,
    pub(super) list: ListState,
    pub(super) rows: Vec<TerminalRow>,
    /// The log version `rows` were made from.
    version: Option<u64>,
    /// The log generation `docs` and `open_thinking` belong to.
    generation: u64,
    /// Thinking rows you opened, by seq.
    open_thinking: HashSet<u64>,
    /// Its messages read as Markdown.
    docs: DocCache,
}

impl TerminalView {
    pub(super) fn new(session: String) -> Self {
        let list = ListState::new(0, ListAlignment::Top, px(800.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            session,
            list,
            rows: Vec::new(),
            version: None,
            generation: 0,
            open_thinking: HashSet::new(),
            docs: DocCache::default(),
        }
    }
}

impl Workspace {
    /// Opens a terminal session in the main area and subscribes to it, in
    /// place of what was open.
    pub(super) fn open_terminal(
        &mut self,
        session: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.running = None;
        self.flush_agent_save(cx);
        self.settings = None;
        if self.terminal.as_ref().is_some_and(|t| t.session == session) {
            cx.notify();
            return;
        }
        self.leave_for_terminal(cx);
        self.state_mut().watch_terminal(&session);
        self.client.watch_terminal(session.clone());
        self.terminal = Some(TerminalView::new(session));
        self.sync_terminal();
        self.focus_workspace(window, cx);
        cx.notify();
    }

    /// Unsubscribes from the open terminal session, if any.
    pub(super) fn close_terminal(&mut self) {
        if let Some(open) = self.terminal.take() {
            self.state_mut().unwatch_terminal(&open.session);
            self.client.unwatch_terminal(open.session);
        }
    }

    /// Brings the open session's rows in line with its items. Called once
    /// per batch of what crewd sent; does nothing unless the items moved.
    pub(super) fn sync_terminal(&mut self) {
        let Some(open) = &self.terminal else {
            return;
        };
        let state = self.state();
        let version = state.terminal_version(&open.session);
        let generation = state.terminal_generation(&open.session);
        if open.version == Some(version) {
            return;
        }
        let cwd = state
            .terminal_session(&open.session)
            .map_or("", |s| s.cwd.as_str());
        let rows = terminal_rows(state.terminal_items(&open.session), cwd);
        let Some(open) = self.terminal.as_mut() else {
            return;
        };
        if open.generation != generation {
            // A seq may now name another item: forget what was read
            // and opened by seq.
            open.generation = generation;
            open.docs = DocCache::default();
            open.open_thinking.clear();
            open.rows.clear();
        }
        let old = std::mem::replace(&mut open.rows, rows);
        let same = old
            .iter()
            .zip(&open.rows)
            .take_while(|(a, b)| a == b)
            .count();
        if same == 0 {
            open.list.reset(open.rows.len());
        } else {
            open.list.splice(same..old.len(), open.rows.len() - same);
        }
        open.version = Some(version);
    }

    fn toggle_thinking(&mut self, ix: usize, seq: u64, cx: &mut Context<Self>) {
        if let Some(open) = self.terminal.as_mut() {
            if !open.open_thinking.remove(&seq) {
                open.open_thinking.insert(seq);
            }
            open.list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    pub(super) fn render_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = &self.terminal else {
            return div().into_any_element();
        };
        let session = self.state().terminal_session(&open.session);
        let title = session
            .map(|s| s.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Untitled session".into());
        let cwd = session.map(|s| s.cwd.clone()).unwrap_or_default();
        div()
            .id("terminal-view")
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(ui_px(12.))
                    .h(scale::row(44.))
                    .px(scale::chrome(24.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(ui_px(8.))
                                    .min_w_0()
                                    .child(
                                        div()
                                            .id("terminal-title")
                                            .min_w_0()
                                            .truncate()
                                            .text_size(scale::text(14.))
                                            .font_weight(weight(FontWeight::SEMIBOLD))
                                            .aria_label(title.clone())
                                            .child(title)
                                            .test_support(),
                                    )
                                    .child(
                                        chip("terminal")
                                            .id("terminal-chip")
                                            .aria_label("terminal")
                                            .test_support(),
                                    ),
                            )
                            .child(
                                div()
                                    .id("terminal-cwd")
                                    .truncate()
                                    .font_family(code_family())
                                    .text_size(scale::mono(12.))
                                    .text_color(color("text-tertiary"))
                                    .aria_label(cwd.clone())
                                    .child(cut_start(&cwd, CWD_MAX))
                                    .test_support(),
                            ),
                    )
                    .child({
                        let note = "Read only. Reply in your terminal.";
                        div()
                            .id("terminal-note")
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(ui_px(6.))
                            .text_size(scale::text(12.))
                            .text_color(color("text-secondary"))
                            .aria_label(note)
                            .child(
                                Icon::new(IconName::SquareTerminal)
                                    .size(ui_px(12.))
                                    .text_color(color("text-tertiary")),
                            )
                            .child(note)
                            .test_support()
                    }),
            )
            .child(if open.rows.is_empty() {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(color("text-tertiary"))
                    .child("Nothing here yet. What you and Claude write shows up here.")
                    .into_any_element()
            } else {
                div()
                    .id("terminal-list")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            open.list.clone(),
                            cx.processor(|this, ix, _, cx| this.render_terminal_row(ix, cx)),
                        )
                        .size_full()
                        .py(scale::row(12.)),
                    )
                    .test_support()
                    .into_any_element()
            })
            .test_support()
            .into_any_element()
    }

    /// Row `ix` of the open session's list.
    fn render_terminal_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = &self.terminal else {
            return div().into_any_element();
        };
        let Some(row) = open.rows.get(ix) else {
            return div().into_any_element();
        };
        let content: AnyElement = match row {
            TerminalRow::You { text, .. } => {
                return author_row(ix, you_avatar(), "You", None)
                    .child(
                        div()
                            .pl(scale::chrome(24.) + ui_px(INDENT - 24.))
                            .pr(scale::chrome(24.))
                            .line_height(scale::line(20.))
                            .child(
                                div()
                                    .id(("terminal-you", ix))
                                    .aria_label(text.clone())
                                    .child(text.clone())
                                    .test_support(),
                            ),
                    )
                    .into_any_element();
            }
            TerminalRow::Agent => {
                return author_row(ix, monogram(AGENT, 32.), AGENT, Some("claude"))
                    .into_any_element();
            }
            TerminalRow::Said { seq, text } => {
                let doc = open.docs.get(DocKey::Terminal(*seq), text);
                div()
                    .id(("terminal-said", ix))
                    .line_height(scale::line(20.))
                    .aria_label(text.clone())
                    .child(render_doc(&doc, &[], &format!("terminal-md-{seq}")))
                    .test_support()
                    .into_any_element()
            }
            TerminalRow::Thinking { seq, text } => {
                let seq = *seq;
                let shown = open.open_thinking.contains(&seq);
                div()
                    .flex()
                    .flex_col()
                    .gap(ui_px(2.))
                    .text_size(scale::text(12.))
                    .child(
                        div()
                            .id(("terminal-thinking", ix))
                            .flex()
                            .items_center()
                            .gap(ui_px(4.))
                            .cursor_pointer()
                            .text_color(color("text-secondary"))
                            .aria_label("Thinking")
                            .child(
                                Icon::new(if shown {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(ui_px(12.))
                                .text_color(color("text-tertiary")),
                            )
                            .child("Thinking")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_thinking(ix, seq, cx);
                            }))
                            .test_support(),
                    )
                    .when(shown, |el| {
                        el.child(
                            div()
                                .id(("terminal-thought", ix))
                                .pl(ui_px(16.))
                                .italic()
                                .text_color(color("text-tertiary"))
                                .aria_label(text.clone())
                                .child(text.clone())
                                .test_support(),
                        )
                    })
                    .into_any_element()
            }
            TerminalRow::Steps(steps) => div()
                .id(("terminal-steps", ix))
                .flex()
                .flex_col()
                .max_w(ui_px(560.))
                .px(ui_px(12.))
                .py(scale::row(4.))
                .border_1()
                .border_color(color("border-100"))
                .rounded(ui_px(8.))
                .children(steps.iter().enumerate().map(step_row))
                .test_support()
                .into_any_element(),
            TerminalRow::Error(message) => div()
                .flex()
                .items_center()
                .gap(ui_px(6.))
                .text_size(scale::text(12.))
                .text_color(color("danger"))
                .child(Icon::new(IconName::CircleX).size(ui_px(12.)))
                .child(
                    div()
                        .id(("terminal-error", ix))
                        .aria_label(message.clone())
                        .child(message.clone())
                        .test_support(),
                )
                .into_any_element(),
        };
        div()
            .pl(scale::chrome(24.) + ui_px(INDENT - 24.))
            .pr(scale::chrome(24.))
            .pb(scale::row(8.))
            .child(content)
            .into_any_element()
    }
}

/// An avatar, a name and an optional chip: the start of a turn.
fn author_row(ix: usize, avatar: Div, name: &str, runtime: Option<&str>) -> Stateful<Div> {
    div()
        .id(("terminal-row", ix))
        .flex()
        .flex_col()
        .pt(scale::row(8.))
        .pb(scale::row(4.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(ui_px(12.))
                .px(scale::chrome(24.))
                .child(avatar)
                .child(
                    div()
                        .font_weight(weight(FontWeight::SEMIBOLD))
                        .child(name.to_string()),
                )
                .when_some(runtime, |row, runtime| row.child(chip(runtime.to_string()))),
        )
}

fn you_avatar() -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(ui_px(32.))
        .rounded_full()
        .bg(color("border-200"))
        .text_size(scale::text(13.))
        .font_weight(weight(FontWeight::SEMIBOLD))
        .child("Y")
}

#[cfg(test)]
mod tests {
    use super::{TEXT_MAX, TerminalRow, terminal_rows};
    use crate::ui::run_card::StepState;
    use crew_core::claude::transcript::TranscriptItem;
    use crew_core::event::{Event, ToolKind};

    fn said(text: &str) -> TranscriptItem {
        TranscriptItem::Event(Event::Message { text: text.into() })
    }

    fn call(id: &str, name: &str, target: &str) -> TranscriptItem {
        TranscriptItem::Event(Event::ToolCall {
            id: id.into(),
            name: name.into(),
            kind: ToolKind::from_tool_name(name),
            target: Some(target.into()),
            input: serde_json::json!({}),
        })
    }

    fn result(id: &str, is_error: bool) -> TranscriptItem {
        TranscriptItem::Event(Event::ToolResult {
            id: id.into(),
            is_error,
            output: String::new(),
        })
    }

    #[test]
    fn prompts_and_turns_become_rows_like_a_run() {
        let items = [
            TranscriptItem::UserPrompt("Fix the test".into()),
            TranscriptItem::Event(Event::Thinking {
                text: "Hmm.".into(),
            }),
            call("t1", "Read", "/work/demo/src/lib.rs"),
            call("t2", "Bash", "cargo test"),
            result("t1", false),
            result("t2", true),
            said("Fixed **it**."),
            call("t3", "Bash", "cargo test"),
            TranscriptItem::Event(Event::TurnEnd {
                ok: true,
                summary: String::new(),
                duration_ms: 1,
                num_turns: 1,
            }),
            TranscriptItem::UserPrompt("Thanks\u{202e}".into()),
            said("You're welcome."),
        ];
        let rows = terminal_rows(
            items.iter().enumerate().map(|(i, it)| (i as u64, it)),
            "/work/demo",
        );
        let kinds: Vec<String> = rows
            .iter()
            .map(|r| match r {
                TerminalRow::You { text, .. } => format!("you {text}"),
                TerminalRow::Agent => "agent".into(),
                TerminalRow::Said { text, .. } => format!("said {text}"),
                TerminalRow::Thinking { text, .. } => format!("thinking {text}"),
                TerminalRow::Steps(steps) => format!(
                    "steps {}",
                    steps
                        .iter()
                        .map(|s| format!("{} {:?}", s.text(), s.state))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                TerminalRow::Error(e) => format!("error {e}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "you Fix the test",
                "agent",
                "thinking Hmm.",
                "steps Read src/lib.rs Done, Ran cargo test Failed",
                "said Fixed **it**.",
                "steps Running cargo test Running",
                "you Thanks",
                "agent",
                "said You're welcome.",
            ]
        );
        let TerminalRow::Steps(steps) = &rows[3] else {
            panic!("steps");
        };
        assert_eq!(steps[0].state, StepState::Done);
    }

    #[test]
    fn an_edit_counts_its_lines_once_it_worked() {
        let edit = TranscriptItem::Event(Event::ToolCall {
            id: "e1".into(),
            name: "Edit".into(),
            kind: ToolKind::Edit,
            target: Some("/work/demo/a.rs".into()),
            input: serde_json::json!({"old_string": "a", "new_string": "b\nc"}),
        });
        let steps = |items: &[TranscriptItem]| {
            let rows = terminal_rows(
                items.iter().enumerate().map(|(i, it)| (i as u64, it)),
                "/work/demo",
            );
            match rows.last() {
                Some(TerminalRow::Steps(steps)) => steps.clone(),
                other => panic!("{other:?}"),
            }
        };
        let running = steps(std::slice::from_ref(&edit));
        assert_eq!(running[0].text(), "Editing a.rs");
        assert_eq!(running[0].lines, None);
        let done = steps(&[edit.clone(), result("e1", false)]);
        assert_eq!(done[0].text(), "Edited a.rs");
        assert_eq!(done[0].lines, Some((2, 1)));
        let failed = steps(&[edit, result("e1", true)]);
        assert_eq!(failed[0].state, StepState::Failed);
        assert_eq!(failed[0].lines, None);
    }

    #[test]
    fn long_text_is_cut_for_drawing() {
        let long = "x".repeat(TEXT_MAX + 10);
        let items = [said(&long), said("short")];
        let rows = terminal_rows(items.iter().enumerate().map(|(i, it)| (i as u64, it)), "");
        let TerminalRow::Said { text, .. } = &rows[1] else {
            panic!("{rows:?}");
        };
        assert_eq!(text.chars().count(), TEXT_MAX + 1);
        assert!(text.ends_with('…'));
        assert_eq!(
            rows[2],
            TerminalRow::Said {
                seq: 1,
                text: "short".into()
            }
        );
    }

    #[test]
    fn an_answer_without_a_prompt_still_names_claude() {
        let items = [
            said("Resumed."),
            TranscriptItem::Event(Event::Error {
                message: "Overloaded".into(),
            }),
        ];
        let rows = terminal_rows(items.iter().enumerate().map(|(i, it)| (i as u64, it)), "");
        assert_eq!(
            rows,
            [
                TerminalRow::Agent,
                TerminalRow::Said {
                    seq: 0,
                    text: "Resumed.".into()
                },
                TerminalRow::Error("Overloaded".into()),
            ]
        );
    }
}
