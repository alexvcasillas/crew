//! The inspector's Terminal tab: the commands a run ran, with their
//! output. Read only, in a virtual list so a long run stays cheap.

use std::collections::HashMap;

use crew_core::event::{Event, ToolKind};
use crew_core::ids::{RunId, ThreadId};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::spinner;
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::model::LoggedEvent;
use crate::theme::color;

/// Output lines a command shows, from its end, until you ask for all.
pub const TAIL_LINES: usize = 20;
pub(super) const NO_COMMANDS: &str = "No commands yet.";

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandState {
    Running,
    Passed,
    Failed,
}

/// One command a run ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The tool call's id.
    pub id: String,
    pub command: String,
    pub state: CommandState,
    pub output: String,
}

/// The commands `run` ran, in order, each with its result once it came.
pub fn commands(events: &[LoggedEvent], run: RunId) -> Vec<Command> {
    let mut found: Vec<Command> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for logged in events.iter().filter(|e| e.run == run) {
        match &logged.event {
            Event::ToolCall {
                id,
                kind: ToolKind::Run,
                target: Some(command),
                ..
            } => {
                at.insert(id, found.len());
                found.push(Command {
                    id: id.clone(),
                    command: command.clone(),
                    state: CommandState::Running,
                    output: String::new(),
                });
            }
            Event::ToolResult {
                id,
                is_error,
                output,
            } => {
                if let Some(&ix) = at.get(id.as_str()) {
                    found[ix].state = if *is_error {
                        CommandState::Failed
                    } else {
                        CommandState::Passed
                    };
                    found[ix].output = output.clone();
                }
            }
            _ => {}
        }
    }
    found
}

/// The directory `run` started in.
pub fn cwd(events: &[LoggedEvent], run: RunId) -> Option<String> {
    events.iter().find_map(|e| match &e.event {
        Event::SessionStarted { cwd, .. } if e.run == run => Some(cwd.clone()),
        _ => None,
    })
}

/// The last `n` lines of `text` and how many came before them.
pub fn last_lines(text: &str, n: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(n);
    (lines[skip..].join("\n"), skip)
}

impl Workspace {
    /// Redoes the Terminal rows from `run`'s events, keeping the scroll.
    pub(super) fn sync_commands(&mut self, thread: ThreadId, run: Option<RunId>, count: usize) {
        let Some(run) = run else {
            if self.inspector.term_from.is_some() {
                self.inspector.term.clear();
                self.inspector.term_cwd = None;
                self.inspector.term_list.reset(0);
                self.inspector.term_from = None;
            }
            return;
        };
        if self.inspector.term_from != Some((run, count)) {
            let events = self.state().events(thread);
            let rows = commands(events, run);
            let cwd = cwd(events, run);
            let same_run = self.inspector.term_from.map(|(r, _)| r) == Some(run);
            let old = std::mem::replace(&mut self.inspector.term, rows);
            let list = &self.inspector.term_list;
            if same_run {
                let from = old
                    .iter()
                    .zip(&self.inspector.term)
                    .take_while(|(a, b)| a == b)
                    .count();
                list.splice(from..old.len(), self.inspector.term.len() - from);
            } else {
                list.reset(self.inspector.term.len());
                // A run opens at its end, and follows it while it is live.
                list.set_follow_mode(FollowMode::Tail);
            }
            self.inspector.term_cwd = cwd;
            self.inspector.term_from = Some((run, count));
        }
        self.reveal_pending_command();
    }

    /// Scrolls to the command asked for, once the rows have it.
    fn reveal_pending_command(&mut self) {
        let Some((run, id)) = self.inspector.term_reveal.as_ref() else {
            return;
        };
        if self.inspector.term_from.map(|(r, _)| r) != Some(*run) {
            return;
        }
        if let Some(ix) = self.inspector.term.iter().position(|c| &c.id == id) {
            self.inspector.term_list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            self.inspector.term_reveal = None;
        }
    }

    pub(super) fn render_commands(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let cwd = self.inspector.term_cwd.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .children(self.render_picker())
            .child(if self.inspector.term.is_empty() {
                div()
                    .id("terminal-empty")
                    .flex_1()
                    .p(px(16.))
                    .text_size(px(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(NO_COMMANDS)
                    .child(NO_COMMANDS)
                    .test_support()
                    .into_any_element()
            } else {
                div()
                    .id("terminal-list")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            self.inspector.term_list.clone(),
                            cx.processor(|this, ix, _, cx| this.render_command(ix, cx)),
                        )
                        .size_full(),
                    )
                    .test_support()
                    .into_any_element()
            })
            .child(
                div()
                    .id("terminal-footer")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.))
                    .min_h(px(32.))
                    .px(px(16.))
                    .border_t_1()
                    .border_color(color("border-100"))
                    .text_size(px(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(match &cwd {
                        Some(cwd) => format!("Read only. Commands ran in {cwd}."),
                        None => "Read only.".to_string(),
                    })
                    .child("Read only.")
                    .when_some(cwd, |footer, cwd| {
                        footer
                            .child("Commands ran in")
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(mono_font())
                                    .child(cwd),
                            )
                            .child(".")
                    })
                    .test_support(),
            )
    }

    /// Row `ix` of the Terminal list.
    fn render_command(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(cmd) = self.inspector.term.get(ix) else {
            return div().into_any_element();
        };
        let key = self
            .inspector
            .term_from
            .map(|(run, _)| (run, cmd.id.clone()));
        let expanded = key
            .as_ref()
            .is_some_and(|k| self.inspector.term_expanded.contains(k));
        let (shown, earlier) = if expanded {
            (cmd.output.clone(), 0)
        } else {
            last_lines(&cmd.output, TAIL_LINES)
        };
        let long = earlier > 0 || (expanded && cmd.output.lines().count() > TAIL_LINES);
        let (word, token) = match cmd.state {
            CommandState::Running => ("Running", "accent-blue"),
            CommandState::Passed => ("Passed", "success"),
            CommandState::Failed => ("Failed", "danger"),
        };
        let icon = match cmd.state {
            CommandState::Running => {
                spinner(("terminal-spinner", ix), 12., "accent-blue").into_any_element()
            }
            CommandState::Passed => Icon::new(IconName::Check)
                .size(px(12.))
                .text_color(color("success"))
                .into_any_element(),
            CommandState::Failed => Icon::new(IconName::CircleX)
                .size(px(12.))
                .text_color(color("danger"))
                .into_any_element(),
        };
        let prompt = format!("$ {}", cmd.command);
        div()
            .id(("terminal-row", ix))
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(16.))
            .py(px(10.))
            .border_b_1()
            .border_color(color("border-100"))
            .child(
                div()
                    .id(("terminal-command", ix))
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .text_color(color("text-primary"))
                    .aria_label(prompt.clone())
                    .child(prompt)
                    .test_support(),
            )
            .when(!cmd.output.is_empty(), |row| {
                row.child(
                    div()
                        .id(("terminal-output", ix))
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
            })
            .when(long, |row| {
                let (label, key) = (
                    if expanded {
                        "Show less".to_string()
                    } else {
                        format!("Show all ({earlier} earlier lines)")
                    },
                    key.clone(),
                );
                row.child(
                    div()
                        .id(("terminal-show-all", ix))
                        .text_size(px(12.))
                        .text_color(color("accent-blue"))
                        .cursor_pointer()
                        .aria_label(label.clone())
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(key) = key.clone()
                                && !this.inspector.term_expanded.remove(&key)
                            {
                                this.inspector.term_expanded.insert(key);
                            }
                            this.inspector.term_list.remeasure_items(ix..ix + 1);
                            cx.notify();
                        }))
                        .test_support(),
                )
            })
            .child(
                div()
                    .id(("terminal-state", ix))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .text_color(color(token))
                    .aria_label(word)
                    .child(icon)
                    .child(word)
                    .test_support(),
            )
            .test_support()
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use crew_core::event::{Event, ToolKind};
    use crew_core::ids::RunId;

    use super::{CommandState, commands, cwd, last_lines};
    use crate::model::LoggedEvent;

    fn at(run: RunId, seq: u64, event: Event) -> LoggedEvent {
        LoggedEvent::new(run, seq, event)
    }

    fn call(id: &str, name: &str, kind: ToolKind, target: &str) -> Event {
        Event::ToolCall {
            id: id.into(),
            name: name.into(),
            kind,
            target: Some(target.into()),
            input: serde_json::Value::Null,
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
    fn commands_pair_calls_with_results_of_their_run() {
        let (a, b) = (RunId::new(), RunId::new());
        let events = [
            at(
                a,
                1,
                Event::SessionStarted {
                    session_id: "s".into(),
                    model: "m".into(),
                    cwd: "/work/demo".into(),
                },
            ),
            at(a, 2, call("t1", "Bash", ToolKind::Run, "ls")),
            at(a, 3, call("t2", "Read", ToolKind::Read, "a.rs")),
            at(b, 1, call("t1", "Bash", ToolKind::Run, "other")),
            at(a, 4, result("t1", false, "x\ny")),
            at(a, 5, call("t3", "Bash", ToolKind::Run, "false")),
            at(a, 6, result("t3", true, "")),
            at(a, 7, call("t4", "Bash", ToolKind::Run, "sleep 9")),
        ];
        let got: Vec<_> = commands(&events, a)
            .into_iter()
            .map(|c| (c.command, c.state, c.output))
            .collect();
        assert_eq!(
            got,
            [
                ("ls".into(), CommandState::Passed, "x\ny".into()),
                ("false".into(), CommandState::Failed, String::new()),
                ("sleep 9".into(), CommandState::Running, String::new()),
            ]
        );
        assert_eq!(cwd(&events, a).as_deref(), Some("/work/demo"));
        assert_eq!(cwd(&events, b), None);
    }

    #[test]
    fn last_lines_keeps_the_end() {
        assert_eq!(last_lines("a\nb\nc", 2), ("b\nc".to_string(), 1));
        assert_eq!(last_lines("a", 20), ("a".to_string(), 0));
        assert_eq!(last_lines("", 20), (String::new(), 0));
    }
}
