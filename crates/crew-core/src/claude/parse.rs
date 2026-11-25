use std::collections::HashMap;

use crate::event::{Event, ToolKind};
use serde_json::Value;

const MAX_OUTPUT_BYTES: usize = 8192;

/// What the auto mode safety check's refusal says in its tool result.
pub const CLASSIFIER_DENIAL: &str = "denied by the Claude Code auto mode classifier";

/// How the safety check's refusal starts.
const CLASSIFIER_SENTENCE: &str =
    "Permission for this action has been denied by the Claude Code auto mode classifier";

/// The category in a safety check refusal (`Reason: [<Category>]`), or
/// empty if the text has none. `None` if `text` isn't such a refusal: it
/// must start with the refusal's sentence, so a command whose output
/// merely quotes it is not one.
pub fn classifier_reason(text: &str) -> Option<String> {
    if !text.trim_start().starts_with(CLASSIFIER_SENTENCE) {
        return None;
    }
    Some(bracketed_reason(text).unwrap_or_default())
}

/// Like [`classifier_reason`], with the sentence anywhere in `text`. Only
/// for text Claude Code itself says is a denial.
fn quoted_classifier_reason(text: &str) -> Option<String> {
    if !text.contains(CLASSIFIER_DENIAL) {
        return None;
    }
    Some(bracketed_reason(text).unwrap_or_default())
}

fn bracketed_reason(text: &str) -> Option<String> {
    let (_, rest) = text.split_once("Reason: [")?;
    let (category, _) = rest.split_once(']')?;
    Some(category.trim().to_string())
}

/// The reason a `permission_denied` line gives, if any.
fn line_reason(v: &Value) -> Option<String> {
    for key in ["reason", "decision_reason", "message", "error"] {
        let Some(text) = v.get(key).and_then(Value::as_str) else {
            continue;
        };
        if let Some(reason) = quoted_classifier_reason(text) {
            return Some(reason);
        }
        if let Some(reason) = bracketed_reason(text) {
            return Some(reason);
        }
        if matches!(key, "reason" | "decision_reason") && !text.trim().is_empty() {
            let text = text.trim().trim_start_matches('[').trim_end_matches(']');
            return Some(text.trim().to_string());
        }
    }
    None
}

/// Parses one run's stdout line by line. Unlike [`parse_line`] it reports
/// each denial once: a `permission_denied` line, a tool result refused by
/// the auto mode safety check and the result's `permission_denials` list
/// can all be about the same call. A `permission_denied` line without a
/// reason waits for its call's tool result, which may carry one.
#[derive(Debug, Default)]
pub struct StreamParser {
    denied: Vec<(Option<String>, String, Value)>,
    /// Tool calls by id: name and input.
    calls: HashMap<String, (String, Value)>,
    /// Denials by tool use id that wait for their tool result.
    held: Vec<(String, String, Value)>,
    /// Failed results that quote the safety check's refusal without
    /// starting with it, by tool use id, with the reason they give. One
    /// is a refusal only if the result's denial list names it too.
    quoted: HashMap<String, String>,
    /// The model of the session, from its `init` line.
    model: String,
    /// The main agent's last reported context size.
    last_tokens: Option<u64>,
}

impl StreamParser {
    pub fn new() -> StreamParser {
        StreamParser::default()
    }

    fn reported(&self, key: &(Option<String>, String, Value)) -> bool {
        self.denied.iter().any(|seen| match (&key.0, &seen.0) {
            (Some(id), Some(seen_id)) => id == seen_id,
            _ => key.1 == seen.1 && key.2 == seen.2,
        })
    }

    /// Turns one stdout line into events.
    pub fn parse(&mut self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return parse_line(line);
        };
        match (str_at(&v, "type"), str_at(&v, "subtype")) {
            ("system", "permission_denied") => {
                let events = parse_line(line);
                let [Event::PermissionDenied { reason, .. }] = events.as_slice() else {
                    return events;
                };
                let key = denial_key(&v);
                if self.reported(&key) {
                    return vec![];
                }
                match (&key.0, reason) {
                    (Some(id), None) => {
                        self.held.push((id.clone(), key.1, key.2));
                        vec![]
                    }
                    _ => {
                        self.denied.push(key);
                        events
                    }
                }
            }
            ("assistant", _) => {
                let mut events = parse_line(line);
                events.extend(self.assistant_usage(&v));
                for event in &events {
                    if let Event::ToolCall {
                        id, name, input, ..
                    } = event
                    {
                        self.calls.insert(id.clone(), (name.clone(), input.clone()));
                    }
                }
                events
            }
            ("user", _) => {
                let mut events = Vec::new();
                for result in blocks(&v).iter().filter_map(tool_result) {
                    if let Event::ToolResult {
                        id,
                        is_error,
                        output,
                    } = &result
                    {
                        events.extend(self.denial_for_result(id, *is_error, output));
                    }
                    events.push(result);
                }
                events
            }
            ("result", _) => {
                let mut events: Vec<Event> = std::mem::take(&mut self.held)
                    .into_iter()
                    .map(|(id, tool, input)| {
                        self.denied.push((Some(id), tool.clone(), input.clone()));
                        Event::PermissionDenied {
                            tool,
                            input,
                            reason: None,
                        }
                    })
                    .collect();
                events.extend(self.result_usage(&v));
                let parser = &*self;
                events.extend(parse_result(
                    &v,
                    |key| parser.reported(key),
                    |id| id.and_then(|id| parser.quoted.get(id).cloned()),
                ));
                events
            }
            ("system", "init") => {
                self.model = str_at(&v, "model").to_string();
                parse_line(line)
            }
            _ => parse_line(line),
        }
    }

    /// A `Usage` when the main agent's context size changed. Lines from
    /// subagents are about their own context and give nothing.
    fn assistant_usage(&mut self, v: &Value) -> Option<Event> {
        if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
            return None;
        }
        let tokens = context_tokens(v.pointer("/message/usage")?)?;
        if self.last_tokens == Some(tokens) {
            return None;
        }
        self.last_tokens = Some(tokens);
        Some(Event::Usage {
            context_tokens: tokens,
            context_window: None,
        })
    }

    /// The `Usage` a `result` line gives: the last context size with the
    /// window of the main model, the one that started the session.
    fn result_usage(&self, v: &Value) -> Option<Event> {
        let models = v.get("modelUsage")?.as_object()?;
        let model = self.model.as_str();
        let main = models.iter().find(|(name, _)| {
            !model.is_empty() && (name.starts_with(model) || model.starts_with(name.as_str()))
        })?;
        let window = main.1.get("contextWindow").and_then(Value::as_u64);
        let tokens = self.last_tokens?;
        Some(Event::Usage {
            context_tokens: tokens,
            context_window: window,
        })
    }

    /// The denial a tool result reports: one held for its call, or a
    /// refusal by the safety check. Nothing if it was reported already.
    fn denial_for_result(&mut self, id: &str, is_error: bool, output: &str) -> Option<Event> {
        let held = self.held.iter().position(|(held, ..)| held == id);
        // A denial line for the call vouches for a refusal quoted anywhere.
        let reason = match held {
            Some(_) => is_error.then(|| quoted_classifier_reason(output)).flatten(),
            None => is_error.then(|| classifier_reason(output)).flatten(),
        };
        let (tool, input) = match held {
            Some(at) => {
                let (_, tool, input) = self.held.remove(at);
                (tool, input)
            }
            None if reason.is_some() => self.calls.get(id).cloned()?,
            None => {
                if let Some(quoted) = is_error.then(|| quoted_classifier_reason(output)).flatten() {
                    self.quoted.insert(id.to_string(), quoted);
                }
                return None;
            }
        };
        let key = (Some(id.to_string()), tool.clone(), input.clone());
        if self.reported(&key) {
            return None;
        }
        self.denied.push(key);
        Some(Event::PermissionDenied {
            tool,
            input,
            reason,
        })
    }
}

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
            "init" => {
                let mut events = vec![Event::SessionStarted {
                    session_id: str_at(&v, "session_id").to_string(),
                    model: str_at(&v, "model").to_string(),
                    cwd: str_at(&v, "cwd").to_string(),
                }];
                // Commands are listed apart from skills; both can be run.
                let listed = |key: &str| {
                    v.get(key).and_then(Value::as_array).map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                };
                let (skills, commands) = (listed("skills"), listed("slash_commands"));
                if skills.is_some() || commands.is_some() {
                    let mut all = skills.unwrap_or_default();
                    for name in commands.unwrap_or_default() {
                        if !all.contains(&name) {
                            all.push(name);
                        }
                    }
                    events.push(Event::SkillsListed { skills: all });
                }
                events
            }
            "hook_started" | "hook_response" | "thinking_tokens" | "commands_changed" => vec![],
            "permission_denied" if !str_at(&v, "tool_name").is_empty() => {
                vec![Event::PermissionDenied {
                    tool: str_at(&v, "tool_name").to_string(),
                    input: v.get("tool_input").cloned().unwrap_or(Value::Null),
                    reason: line_reason(&v),
                }]
            }
            _ => raw(),
        },
        "rate_limit_event" => vec![],
        "assistant" => blocks(&v).iter().filter_map(assistant_block).collect(),
        "user" => blocks(&v).iter().filter_map(tool_result).collect(),
        "result" => parse_result(&v, |_| false, |_| None),
        _ => raw(),
    }
}

pub(crate) fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn blocks(v: &Value) -> &[Value] {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub(crate) fn assistant_block(b: &Value) -> Option<Event> {
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

pub(crate) fn tool_result(b: &Value) -> Option<Event> {
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

pub(crate) fn truncate(mut s: String) -> String {
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

/// Tokens in the context an API call read: input plus both cache kinds.
fn context_tokens(usage: &Value) -> Option<u64> {
    let n = |k: &str| usage.get(k).and_then(Value::as_u64);
    let total = n("input_tokens")?
        + n("cache_read_input_tokens").unwrap_or(0)
        + n("cache_creation_input_tokens").unwrap_or(0);
    Some(total)
}

/// Tool use id (when present), tool name and input of one denial.
fn denial_key(v: &Value) -> (Option<String>, String, Value) {
    let id = Some(str_at(v, "tool_use_id")).filter(|id| !id.is_empty());
    (
        id.map(str::to_string),
        str_at(v, "tool_name").to_string(),
        v.get("tool_input").cloned().unwrap_or(Value::Null),
    )
}

/// Events for a `result` line, skipping denials `seen` says were reported.
/// `reason_of` gives the safety check's reason for a denial by tool use
/// id, if its result quoted one.
fn parse_result(
    v: &Value,
    seen: impl Fn(&(Option<String>, String, Value)) -> bool,
    reason_of: impl Fn(Option<&str>) -> Option<String>,
) -> Vec<Event> {
    let mut events: Vec<Event> = v
        .get("permission_denials")
        .and_then(Value::as_array)
        .map(|d| {
            d.iter()
                .map(denial_key)
                .filter(|key| !seen(key))
                .map(|(id, tool, input)| Event::PermissionDenied {
                    tool,
                    input,
                    reason: reason_of(id.as_deref()),
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
