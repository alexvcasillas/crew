//! The inspector's Terminal tab: the commands a run ran, with their
//! output. Read only, in a virtual list so a long run stays cheap.

use std::collections::HashMap;

use crew_core::event::{Event, ToolKind};
use crew_core::ids::{RunId, ThreadId};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::spinner;
use super::inspector::InspectorTab;
use super::workspace::Workspace;
use crate::model::LoggedEvent;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px};

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

/// The commands of one run and where they are, built event by event.
#[derive(Debug, Default)]
pub struct CommandLog {
    pub rows: Vec<Command>,
    /// Call id to its row.
    index: HashMap<String, usize>,
    /// The directory the run started in.
    pub cwd: Option<String>,
}

/// What an event did to a [`CommandLog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Touched {
    Nothing,
    Added,
    /// The row at this index got its result.
    Patched(usize),
}

impl CommandLog {
    /// Takes in one event of the run.
    pub fn push(&mut self, event: &Event) -> Touched {
        match event {
            Event::SessionStarted { cwd, .. } => {
                self.cwd.get_or_insert_with(|| cwd.clone());
                Touched::Nothing
            }
            Event::ToolCall {
                id,
                kind: ToolKind::Run,
                target: Some(command),
                ..
            } => {
                self.index.insert(id.clone(), self.rows.len());
                self.rows.push(Command {
                    id: id.clone(),
                    command: command.clone(),
                    state: CommandState::Running,
                    output: String::new(),
                });
                Touched::Added
            }
            Event::ToolResult {
                id,
                is_error,
                output,
            } => match self.index.get(id) {
                Some(&ix) => {
                    let row = &mut self.rows[ix];
                    row.state = if *is_error {
                        CommandState::Failed
                    } else {
                        CommandState::Passed
                    };
                    row.output = output.clone();
                    Touched::Patched(ix)
                }
                None => Touched::Nothing,
            },
            _ => Touched::Nothing,
        }
    }
}

/// The commands `run` ran, in order, each with its result once it came.
pub fn commands(events: &[LoggedEvent], run: RunId) -> CommandLog {
    let mut log = CommandLog::default();
    for logged in events.iter().filter(|e| e.run == run) {
        log.push(&logged.event);
    }
    log
}

/// The last `n` lines of `text` and how many came before them.
pub fn last_lines(text: &str, n: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(n);
    (lines[skip..].join("\n"), skip)
}

impl Workspace {
    /// Brings the Terminal rows in line with `run`'s events, taking in
    /// only the events that came since. Does nothing while another tab
    /// is shown: the rows catch up when you switch to this one.
    pub(super) fn sync_commands(&mut self, thread: ThreadId, run: Option<RunId>) {
        if self.inspector.tab != InspectorTab::Terminal {
            return;
        }
        let Some(run) = run else {
            self.inspector.term_thread = None;
            if self.inspector.term_run.take().is_some() {
                self.inspector.term = CommandLog::default();
                self.inspector.term_done = 0;
                self.inspector.term_list.reset(0);
            }
            self.inspector.term_reveal = None;
            return;
        };
        let live = self.state().live_runs(thread).iter().any(|r| r.id == run);
        let mut term = std::mem::take(&mut self.inspector.term);
        let (mut done, was) = (self.inspector.term_done, self.inspector.term_run);
        let (was_thread, was_last) = (self.inspector.term_thread, self.inspector.term_last);
        let list = self.inspector.term_list.clone();
        let events = self.state().events(thread);
        // The events taken in must still be the log's first `done`: the
        // log is dropped and replayed when the thread is left and opened
        // again.
        let last_of = |n: usize| n.checked_sub(1).map(|i| (events[i].run, events[i].seq));
        let same_log = was_thread == Some(thread) && was_last == last_of(done);
        if was != Some(run) || events.len() < done || !same_log {
            term = commands(events, run);
            list.reset(term.rows.len());
            // A run opens at its end, and follows it while it is live.
            list.set_follow_mode(FollowMode::Tail);
        } else if events.len() > done {
            let before = term.rows.len();
            for logged in events[done..].iter().filter(|e| e.run == run) {
                if let Touched::Patched(ix) = term.push(&logged.event)
                    && ix < before
                {
                    list.remeasure_items(ix..ix + 1);
                }
            }
            if term.rows.len() > before {
                list.splice(before..before, term.rows.len() - before);
            }
        }
        done = events.len();
        let last = last_of(done);
        self.inspector.term = term;
        self.inspector.term_done = done;
        self.inspector.term_thread = Some(thread);
        self.inspector.term_last = last;
        self.inspector.term_live = live;
        self.inspector.term_run = Some(run);
        self.inspector.term_expanded.retain(|(r, _)| *r == run);
        self.reveal_pending_command();
    }

    /// Scrolls to the command asked for. A command the shown run does
    /// not have is forgotten.
    fn reveal_pending_command(&mut self) {
        let Some((run, id)) = self.inspector.term_reveal.take() else {
            return;
        };
        if self.inspector.term_run != Some(run) {
            return;
        }
        if let Some(ix) = self.inspector.term.rows.iter().position(|c| c.id == id) {
            self.inspector.term_list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: ui_px(0.),
            });
        }
    }

    pub(super) fn render_commands(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let cwd = self.inspector.term.cwd.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .children(self.render_picker())
            .child(if self.inspector.term.rows.is_empty() {
                div()
                    .id("terminal-empty")
                    .flex_1()
                    .p(ui_px(16.))
                    .text_size(scale::text(12.))
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
                    .gap(ui_px(4.))
                    .min_h(scale::row(32.))
                    .px(ui_px(16.))
                    .border_t_1()
                    .border_color(color("border-100"))
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(match &cwd {
                        Some(cwd) => format!("Read only. Commands ran in {cwd}"),
                        None => "Read only.".to_string(),
                    })
                    .child("Read only.")
                    .when_some(cwd, |footer, cwd| {
                        footer.child("Commands ran in").child(
                            div()
                                .flex()
                                .min_w_0()
                                .child(div().truncate().font_family(code_family()).child(cwd)),
                        )
                    })
                    .test_support(),
            )
    }

    /// Row `ix` of the Terminal list.
    fn render_command(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(cmd) = self.inspector.term.rows.get(ix) else {
            return div().into_any_element();
        };
        let key = self.inspector.term_run.map(|run| (run, cmd.id.clone()));
        let expanded = key
            .as_ref()
            .is_some_and(|k| self.inspector.term_expanded.contains(k));
        let (shown, earlier) = if expanded {
            (cmd.output.clone(), 0)
        } else {
            last_lines(&cmd.output, TAIL_LINES)
        };
        let long = earlier > 0 || (expanded && cmd.output.lines().count() > TAIL_LINES);
        // A command with no result in a run that is over never finished.
        let unfinished = cmd.state == CommandState::Running && !self.inspector.term_live;
        let (word, token) = match cmd.state {
            CommandState::Running if unfinished => ("Didn't finish", "text-tertiary"),
            CommandState::Running => ("Running", "accent-blue"),
            CommandState::Passed => ("Done", "success"),
            CommandState::Failed => ("Failed", "danger"),
        };
        let icon = match cmd.state {
            CommandState::Running if unfinished => Icon::new(IconName::Minus)
                .size(ui_px(12.))
                .text_color(color("text-tertiary"))
                .into_any_element(),
            CommandState::Running => {
                spinner(("terminal-spinner", ix), 12., "accent-blue").into_any_element()
            }
            CommandState::Passed => Icon::new(IconName::Check)
                .size(ui_px(12.))
                .text_color(color("success"))
                .into_any_element(),
            CommandState::Failed => Icon::new(IconName::CircleX)
                .size(ui_px(12.))
                .text_color(color("danger"))
                .into_any_element(),
        };
        let prompt = format!("$ {}", cmd.command);
        div()
            .id(("terminal-row", ix))
            .flex()
            .flex_col()
            .gap(ui_px(6.))
            .px(ui_px(16.))
            .py(scale::row(10.))
            .border_b_1()
            .border_color(color("border-100"))
            .child(
                div()
                    .id(("terminal-command", ix))
                    .font_family(code_family())
                    .text_size(scale::mono(12.))
                    .text_color(color("text-primary"))
                    .aria_label(prompt.clone())
                    .child(prompt)
                    .test_support(),
            )
            .when(!cmd.output.is_empty(), |row| {
                row.child(
                    div()
                        .id(("terminal-output", ix))
                        .px(ui_px(8.))
                        .py(scale::row(6.))
                        .bg(color("bg-100"))
                        .border_1()
                        .border_color(color("border-100"))
                        .rounded(ui_px(6.))
                        .font_family(code_family())
                        .text_size(scale::mono(11.5))
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
                        .text_size(scale::text(12.))
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
                    .gap(ui_px(6.))
                    .text_size(scale::text(12.))
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

    use super::{CommandLog, CommandState, Touched, commands, last_lines};
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
        let log = commands(&events, a);
        let got: Vec<_> = log
            .rows
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
        assert_eq!(commands(&events, a).cwd.as_deref(), Some("/work/demo"));
        assert_eq!(commands(&events, b).cwd, None);
    }

    #[test]
    fn a_log_grows_one_event_at_a_time() {
        let mut log = CommandLog::default();
        assert_eq!(
            log.push(&call("t1", "Bash", ToolKind::Run, "ls")),
            Touched::Added
        );
        assert_eq!(
            log.push(&call("t2", "Read", ToolKind::Read, "a")),
            Touched::Nothing
        );
        assert_eq!(log.push(&result("t1", false, "out")), Touched::Patched(0));
        assert_eq!(log.push(&result("zz", false, "")), Touched::Nothing);
        assert_eq!(log.rows[0].output, "out");
    }

    #[test]
    fn last_lines_keeps_the_end() {
        assert_eq!(last_lines("a\nb\nc", 2), ("b\nc".to_string(), 1));
        assert_eq!(last_lines("a", 20), ("a".to_string(), 0));
        assert_eq!(last_lines("", 20), (String::new(), 0));
    }
}
