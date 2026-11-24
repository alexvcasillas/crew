use serde::{Deserialize, Serialize};

/// Rough category of a tool call, used to render it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Search,
    Edit,
    Create,
    Run,
    Other,
}

impl ToolKind {
    /// Maps a Claude Code tool name to a kind.
    pub fn from_tool_name(name: &str) -> ToolKind {
        match name {
            "Read" => ToolKind::Read,
            "Grep" | "Glob" => ToolKind::Search,
            "Edit" | "MultiEdit" | "NotebookEdit" => ToolKind::Edit,
            "Write" => ToolKind::Create,
            "Bash" => ToolKind::Run,
            _ => ToolKind::Other,
        }
    }
}

/// One thing an agent did or said during a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted {
        session_id: String,
        model: String,
        cwd: String,
    },
    Message {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        kind: ToolKind,
        target: Option<String>,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        is_error: bool,
        output: String,
    },
    /// A tool call that was refused. `reason` is set when Claude Code's
    /// auto mode safety check stopped it: the category it gave, such as
    /// `Destructive or unwanted actions`, or empty if it gave none.
    PermissionDenied {
        tool: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    TurnEnd {
        ok: bool,
        summary: String,
        duration_ms: u64,
        num_turns: u32,
    },
    Error {
        message: String,
    },
    Raw {
        line: String,
    },
}

/// A safety check category as a short sentence: `[destructive_action]`
/// or `DestructiveAction` reads `Destructive action`.
pub fn humanize_reason(raw: &str) -> String {
    let raw = raw.trim().trim_start_matches('[').trim_end_matches(']');
    let mut spaced = String::new();
    let mut prev_lower = false;
    for c in raw.chars() {
        if c == '_' || c == '-' {
            spaced.push(' ');
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower {
            spaced.push(' ');
        }
        prev_lower = c.is_lowercase();
        spaced.push(c);
    }
    let words = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// Lifecycle state of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Working,
    Done,
    Failed,
    Stopped,
    Interrupted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trips_through_json() {
        let events = vec![
            Event::SessionStarted {
                session_id: "s1".into(),
                model: "m".into(),
                cwd: "/work".into(),
            },
            Event::Message { text: "hi".into() },
            Event::Thinking { text: "hmm".into() },
            Event::ToolCall {
                id: "t1".into(),
                name: "Edit".into(),
                kind: ToolKind::Edit,
                target: Some("src/main.rs".into()),
                input: serde_json::json!({"file_path": "src/main.rs"}),
            },
            Event::ToolResult {
                id: "t1".into(),
                is_error: false,
                output: "ok".into(),
            },
            Event::PermissionDenied {
                tool: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                reason: None,
            },
            Event::PermissionDenied {
                tool: "Bash".into(),
                input: serde_json::json!({"command": "rm -rf x"}),
                reason: Some("Destructive or unwanted actions".into()),
            },
            Event::TurnEnd {
                ok: true,
                summary: "done".into(),
                duration_ms: 12,
                num_turns: 3,
            },
            Event::Error {
                message: "boom".into(),
            },
            Event::Raw { line: "x".into() },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(event, back);
        }

        let call = Event::ToolCall {
            id: "t1".into(),
            name: "Edit".into(),
            kind: ToolKind::Edit,
            target: None,
            input: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&call).unwrap();
        assert!(json.contains(r#""type":"tool_call""#));
        assert!(json.contains(r#""kind":"edit""#));
    }

    #[test]
    fn an_older_denial_has_no_reason() {
        let old: Event = serde_json::from_str(
            r#"{"type":"permission_denied","tool":"Bash","input":{"command":"ls"}}"#,
        )
        .unwrap();
        assert_eq!(
            old,
            Event::PermissionDenied {
                tool: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                reason: None,
            }
        );
    }

    #[test]
    fn reasons_read_as_a_sentence() {
        assert_eq!(
            humanize_reason("[Destructive or unwanted actions]"),
            "Destructive or unwanted actions"
        );
        assert_eq!(humanize_reason("destructive_action"), "Destructive action");
        assert_eq!(humanize_reason("DataExfiltration"), "Data exfiltration");
        assert_eq!(humanize_reason("  "), "");
    }

    #[test]
    fn tool_kind_maps_claude_tools() {
        assert_eq!(ToolKind::from_tool_name("Read"), ToolKind::Read);
        assert_eq!(ToolKind::from_tool_name("Grep"), ToolKind::Search);
        assert_eq!(ToolKind::from_tool_name("Glob"), ToolKind::Search);
        assert_eq!(ToolKind::from_tool_name("Edit"), ToolKind::Edit);
        assert_eq!(ToolKind::from_tool_name("MultiEdit"), ToolKind::Edit);
        assert_eq!(ToolKind::from_tool_name("NotebookEdit"), ToolKind::Edit);
        assert_eq!(ToolKind::from_tool_name("Write"), ToolKind::Create);
        assert_eq!(ToolKind::from_tool_name("Bash"), ToolKind::Run);
        assert_eq!(ToolKind::from_tool_name("WebFetch"), ToolKind::Other);
    }
}
