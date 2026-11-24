//! Turns `codex app-server` messages into Crew [`Event`]s.
//!
//! [`CodexTranslator`] is pure: it reads [`Incoming`] messages one by one and
//! returns the events they stand for. It keeps just enough state to pair
//! tool results with their calls and to remember the final answer.

use serde_json::{Value, json};

use super::client::Incoming;
use super::proto::{
    AgentMessageItem, CommandExecutionItem, FileChange, FileChangeItem, ItemNotification,
    ItemStatus, McpToolCallItem, ServerNotification, ThreadItem, TurnCompleted, TurnStatus,
};
use crate::claude::parse::truncate;
use crate::event::{Event, ToolKind};

/// Notifications that carry no chat content. They are dropped, not shown raw.
const QUIET: &[&str] = &[
    "thread/status/changed",
    "thread/tokenUsage/updated",
    "thread/readState/changed",
    "thread/settings/updated",
    "thread/goal/cleared",
    "turn/started",
    "turn/moderationMetadata",
    "account/updated",
    "account/rateLimits/updated",
    "remoteControl/status/changed",
    "mcpServer/startupStatus/updated",
    "serverRequest/resolved",
];

/// Translates one Codex thread's messages. Create one per run.
#[derive(Debug, Default)]
pub struct CodexTranslator {
    /// Items started but not completed: item id and the tool call ids it made.
    open: Vec<(String, Vec<String>)>,
    /// Text of the last `final_answer` message of the current turn.
    final_answer: Option<String>,
}

impl CodexTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Events for one incoming message. Server requests (approvals) are not
    /// events and give nothing here.
    pub fn on(&mut self, msg: &Incoming) -> Vec<Event> {
        let Incoming::Notification(n) = msg else {
            return vec![];
        };
        match n {
            ServerNotification::ItemStarted(n) => self.item_started(n),
            ServerNotification::ItemCompleted(n) => self.item_completed(n),
            ServerNotification::TurnCompleted(t) => self.turn_completed(t),
            // Text arrives whole on `item/completed`; diffs come from git.
            ServerNotification::AgentMessageDelta(_) | ServerNotification::TurnDiffUpdated(_) => {
                vec![]
            }
            ServerNotification::Raw(v) => raw_notification(v),
        }
    }

    /// Ends every item that was started but never completed, for example
    /// after an interrupt, with an error result carrying `reason`.
    pub fn close_open(&mut self, reason: &str) -> Vec<Event> {
        std::mem::take(&mut self.open)
            .into_iter()
            .flat_map(|(_, calls)| calls)
            .map(|id| Event::ToolResult {
                id,
                is_error: true,
                output: reason.to_string(),
            })
            .collect()
    }

    fn item_started(&mut self, n: &ItemNotification) -> Vec<Event> {
        match &n.item {
            ThreadItem::CommandExecution(c) => self.open_item(&c.id, command_calls(c)),
            ThreadItem::FileChange(f) => self.open_item(&f.id, file_calls(f)),
            ThreadItem::McpToolCall(m) => self.open_item(&m.id, vec![mcp_call(m)]),
            ThreadItem::AgentMessage(_) | ThreadItem::Other(_) => vec![],
        }
    }

    fn item_completed(&mut self, n: &ItemNotification) -> Vec<Event> {
        match &n.item {
            ThreadItem::AgentMessage(m) => self.agent_message(m),
            ThreadItem::CommandExecution(c) => {
                let mut events = self.ensure_open(&c.id, || command_calls(c));
                let id = c.id.clone();
                let failed = matches!(c.status, ItemStatus::Failed | ItemStatus::Declined)
                    || c.exit_code.is_some_and(|code| code != 0);
                let mut output = c.aggregated_output.clone().unwrap_or_default();
                if output.is_empty() && c.status == ItemStatus::Declined {
                    output = "Declined".into();
                }
                events.push(result(id, failed, output));
                self.close_item(&c.id);
                events
            }
            ThreadItem::FileChange(f) => {
                let mut events = self.ensure_open(&f.id, || file_calls(f));
                let failed = matches!(f.status, ItemStatus::Failed | ItemStatus::Declined);
                let output = match f.status {
                    ItemStatus::Failed => "Not applied",
                    ItemStatus::Declined => "Declined",
                    _ => "Applied",
                };
                events.extend(
                    (0..f.changes.len()).map(|i| result(call_id(&f.id, i), failed, output.into())),
                );
                self.close_item(&f.id);
                events
            }
            ThreadItem::McpToolCall(m) => {
                let mut events = self.ensure_open(&m.id, || vec![mcp_call(m)]);
                let failed = matches!(m.status, ItemStatus::Failed | ItemStatus::Declined)
                    || m.error.is_some();
                events.push(result(m.id.clone(), failed, mcp_output(m)));
                self.close_item(&m.id);
                events
            }
            ThreadItem::Other(v) => match v.get("type").and_then(Value::as_str) {
                Some("userMessage") => vec![],
                _ => vec![Event::Raw {
                    line: v.to_string(),
                }],
            },
        }
    }

    fn agent_message(&mut self, m: &AgentMessageItem) -> Vec<Event> {
        if m.phase.as_deref() == Some("final_answer") {
            self.final_answer = Some(m.text.clone());
            return vec![];
        }
        if m.text.is_empty() {
            return vec![];
        }
        vec![Event::Message {
            text: m.text.clone(),
        }]
    }

    fn turn_completed(&mut self, t: &TurnCompleted) -> Vec<Event> {
        let streamed = self.final_answer.take();
        let ok = t.turn.status == TurnStatus::Completed;
        let mut events = Vec::new();
        if t.turn.status == TurnStatus::Failed
            && let Some(e) = t.turn.error.as_ref().filter(|e| !e.message.is_empty())
        {
            events.push(Event::Error {
                message: e.message.clone(),
            });
        }
        let summary = if ok {
            streamed
                .or_else(|| {
                    // The turn's own summary view holds the final message.
                    t.turn.items.iter().rev().find_map(|i| match i {
                        ThreadItem::AgentMessage(m)
                            if m.phase.as_deref() == Some("final_answer") =>
                        {
                            Some(m.text.clone())
                        }
                        _ => None,
                    })
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        events.push(Event::TurnEnd {
            ok,
            summary,
            duration_ms: t.turn.duration_ms.unwrap_or(0),
            num_turns: 1,
        });
        events
    }

    /// Records a started item and returns its calls. A repeated start is
    /// ignored.
    fn open_item(&mut self, id: &str, calls: Vec<Event>) -> Vec<Event> {
        if self.open.iter().any(|(open, _)| open == id) {
            return vec![];
        }
        let ids = calls.iter().filter_map(call_event_id).collect();
        self.open.push((id.to_string(), ids));
        calls
    }

    /// Calls for an item that completed without an `item/started` first.
    fn ensure_open(&mut self, id: &str, calls: impl FnOnce() -> Vec<Event>) -> Vec<Event> {
        if self.open.iter().any(|(open, _)| open == id) {
            vec![]
        } else {
            self.open_item(id, calls())
        }
    }

    fn close_item(&mut self, id: &str) {
        self.open.retain(|(open, _)| open != id);
    }
}

fn result(id: String, is_error: bool, output: String) -> Event {
    Event::ToolResult {
        id,
        is_error,
        output: truncate(output),
    }
}

fn call_event_id(e: &Event) -> Option<String> {
    match e {
        Event::ToolCall { id, .. } => Some(id.clone()),
        _ => None,
    }
}

fn command_calls(c: &CommandExecutionItem) -> Vec<Event> {
    vec![Event::ToolCall {
        id: c.id.clone(),
        name: "Bash".into(),
        kind: ToolKind::Run,
        target: Some(c.command.clone()),
        input: json!({"command": c.command}),
    }]
}

/// The first file keeps the item id, so an approval for the item points at it.
fn call_id(item_id: &str, index: usize) -> String {
    if index == 0 {
        item_id.to_string()
    } else {
        format!("{item_id}#{index}")
    }
}

fn file_calls(f: &FileChangeItem) -> Vec<Event> {
    f.changes
        .iter()
        .enumerate()
        .map(|(i, c)| file_call(call_id(&f.id, i), c))
        .collect()
}

fn file_call(id: String, c: &FileChange) -> Event {
    let added = c.kind.get("type").and_then(Value::as_str) == Some("add");
    let (name, kind) = if added {
        ("Write", ToolKind::Create)
    } else {
        ("Edit", ToolKind::Edit)
    };
    Event::ToolCall {
        id,
        name: name.into(),
        kind,
        target: Some(c.path.clone()),
        input: json!({"path": c.path, "diff": c.diff}),
    }
}

fn mcp_call(m: &McpToolCallItem) -> Event {
    Event::ToolCall {
        id: m.id.clone(),
        name: format!("mcp__{}__{}", m.server, m.tool),
        kind: ToolKind::Other,
        target: None,
        input: m.arguments.clone(),
    }
}

fn mcp_output(m: &McpToolCallItem) -> String {
    if let Some(e) = &m.error {
        return match e.get("message").and_then(Value::as_str) {
            Some(msg) => msg.to_string(),
            None => e.as_str().map_or_else(|| e.to_string(), str::to_string),
        };
    }
    let Some(r) = &m.result else {
        return String::new();
    };
    match r.get("content").and_then(Value::as_array) {
        Some(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        None => r.to_string(),
    }
}

fn raw_notification(v: &Value) -> Vec<Event> {
    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
    if method == "thread/started" {
        let thread = &v["params"]["thread"];
        let text = |k: &str| {
            thread
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        return vec![Event::SessionStarted {
            session_id: text("id"),
            model: text("model"),
            cwd: text("cwd"),
        }];
    }
    if QUIET.contains(&method) {
        return vec![];
    }
    vec![Event::Raw {
        line: v.to_string(),
    }]
}
