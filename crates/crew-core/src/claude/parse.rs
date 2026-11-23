use crate::event::{Event, ToolKind};
use serde_json::Value;

const MAX_OUTPUT_BYTES: usize = 8192;

/// Turns one stdout line of `claude -p --output-format stream-json --verbose`
/// into events. A line can carry several content blocks.
pub fn parse_line(line: &str) -> Vec<Event> {
    let raw = || {
        vec![Event::Raw {
            line: line.to_string(),
        }]
    };
    if line.trim().is_empty() {
        return vec![];
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return raw();
    };
    match str_at(&v, "type") {
        "system" => match str_at(&v, "subtype") {
            "init" => vec![Event::SessionStarted {
                session_id: str_at(&v, "session_id").to_string(),
                model: str_at(&v, "model").to_string(),
                cwd: str_at(&v, "cwd").to_string(),
            }],
            "hook_started" | "hook_response" => vec![],
            _ => raw(),
        },
        "rate_limit_event" => vec![],
        "assistant" => blocks(&v).iter().filter_map(assistant_block).collect(),
        "user" => blocks(&v).iter().filter_map(tool_result).collect(),
        "result" => result_events(&v),
        _ => raw(),
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn blocks(v: &Value) -> &[Value] {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn assistant_block(b: &Value) -> Option<Event> {
    match str_at(b, "type") {
        "text" => Some(Event::Message {
            text: str_at(b, "text").to_string(),
        }),
        "thinking" => Some(Event::Thinking {
            text: str_at(b, "thinking").to_string(),
        }),
        "tool_use" => {
            let name = str_at(b, "name");
            let input = b.get("input").cloned().unwrap_or(Value::Null);
            let target = ["file_path", "pattern", "command", "url"]
                .iter()
                .find_map(|k| input.get(*k).and_then(Value::as_str))
                .map(str::to_string);
            Some(Event::ToolCall {
                id: str_at(b, "id").to_string(),
                name: name.to_string(),
                kind: ToolKind::from_tool_name(name),
                target,
                input,
            })
        }
        _ => None,
    }
}

fn tool_result(b: &Value) -> Option<Event> {
    if str_at(b, "type") != "tool_result" {
        return None;
    }
    let text = match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| str_at(p, "type") == "text")
            .map(|p| str_at(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    Some(Event::ToolResult {
        id: str_at(b, "tool_use_id").to_string(),
        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        output: truncate(text),
    })
}

fn truncate(mut s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("\n… (truncated)");
    s
}

fn result_events(v: &Value) -> Vec<Event> {
    let mut events: Vec<Event> = v
        .get("permission_denials")
        .and_then(Value::as_array)
        .map(|d| {
            d.iter()
                .map(|p| Event::PermissionDenied {
                    tool: str_at(p, "tool_name").to_string(),
                    input: p.get("tool_input").cloned().unwrap_or(Value::Null),
                })
                .collect()
        })
        .unwrap_or_default();
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    events.push(Event::TurnEnd {
        ok: str_at(v, "subtype") == "success" && !is_error,
        summary: str_at(v, "result").to_string(),
        duration_ms: v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0),
        num_turns: v
            .get("num_turns")
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .unwrap_or(0),
    });
    events
}
