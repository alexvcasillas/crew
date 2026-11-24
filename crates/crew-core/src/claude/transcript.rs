//! Parsing of the session files Claude Code writes under
//! `~/.claude/projects`, for a read-only view of terminal sessions.
//!
//! Unlike `claude -p` output, these files have no `init` or `result` line,
//! carry the typed prompt as a plain string, put one content block on each
//! assistant line and hold many line types that are not part of the
//! conversation. Those give nothing.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::parse::{assistant_block, blocks, str_at, tool_result};
use crate::event::Event;

/// Longest title, in characters.
const TITLE_MAX: usize = 80;

/// One thing shown in a terminal session's transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptItem {
    /// What the person typed.
    UserPrompt(String),
    /// What the agent did or said.
    Event(Event),
}

/// Keys read from any line, shown or not.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LineInfo {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// Unix milliseconds.
    pub timestamp: Option<i64>,
    /// How the CLI was started, such as `cli` or `sdk-cli`.
    pub entrypoint: Option<String>,
}

/// One parsed line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedLine {
    pub info: LineInfo,
    pub items: Vec<TranscriptItem>,
}

/// Turns one line of a session file into its items.
pub fn parse_line(line: &str) -> Vec<TranscriptItem> {
    parse(line).items
}

/// Turns one line of a session file into its keys and items.
pub fn parse(line: &str) -> ParsedLine {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return ParsedLine::default();
    };
    if !v.is_object() {
        return ParsedLine::default();
    }
    ParsedLine {
        info: info(&v),
        items: items(&v),
    }
}

fn non_empty(v: &Value, key: &str) -> Option<String> {
    Some(str_at(v, key))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn info(v: &Value) -> LineInfo {
    LineInfo {
        session_id: non_empty(v, "sessionId").or_else(|| non_empty(v, "session_id")),
        cwd: non_empty(v, "cwd"),
        timestamp: chrono::DateTime::parse_from_rfc3339(str_at(v, "timestamp"))
            .ok()
            .map(|t| t.timestamp_millis()),
        entrypoint: non_empty(v, "entrypoint"),
    }
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn items(v: &Value) -> Vec<TranscriptItem> {
    // Injected context, subagent turns and compaction summaries are not
    // part of what the person sees in their terminal.
    if flag(v, "isMeta") || flag(v, "isSidechain") || flag(v, "isCompactSummary") {
        return vec![];
    }
    match str_at(v, "type") {
        "user" => match v.pointer("/message/content") {
            Some(Value::String(text)) => prompt(text).into_iter().collect(),
            _ => blocks(v)
                .iter()
                .filter_map(|b| match str_at(b, "type") {
                    "text" => prompt(str_at(b, "text")),
                    _ => tool_result(b).map(TranscriptItem::Event),
                })
                .collect(),
        },
        "assistant" => blocks(v)
            .iter()
            .filter_map(assistant_block)
            .map(|e| match e {
                Event::Message { text } if flag(v, "isApiErrorMessage") => {
                    Event::Error { message: text }
                }
                e => e,
            })
            .map(TranscriptItem::Event)
            .collect(),
        "system" => match str_at(v, "subtype") {
            "turn_duration" => vec![TranscriptItem::Event(Event::TurnEnd {
                ok: true,
                summary: String::new(),
                duration_ms: v.get("durationMs").and_then(Value::as_u64).unwrap_or(0),
                num_turns: 0,
            })],
            "api_error" => vec![TranscriptItem::Event(Event::Error {
                message: api_error(v),
            })],
            _ => vec![],
        },
        _ => vec![],
    }
}

fn prompt(text: &str) -> Option<TranscriptItem> {
    (!text.trim().is_empty() && !is_command_echo(text))
        .then(|| TranscriptItem::UserPrompt(text.to_string()))
}

/// What Claude Code writes for a slash command or its output, not what
/// the person typed.
fn is_command_echo(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<local-command-",
    ]
    .iter()
    .any(|tag| text.starts_with(tag))
}

fn api_error(v: &Value) -> String {
    v.pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| v.get("error").and_then(Value::as_str))
        .or_else(|| v.get("content").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .unwrap_or("API error")
        .to_string()
}

/// A session title from a prompt: its first non-empty line without
/// control characters, at most 80 characters. `None` for a local command
/// echo or a prompt with no text.
pub fn title_from(prompt: &str) -> Option<String> {
    if is_command_echo(prompt) || prompt.trim_start().starts_with("<bash-") {
        return None;
    }
    let line = strip_escapes(prompt)
        .split(['\n', '\r', '\u{2028}', '\u{2029}'])
        .map(clean)
        .find(|l| !l.is_empty())?;
    if line.chars().count() <= TITLE_MAX {
        return Some(line);
    }
    let mut short: String = line.chars().take(TITLE_MAX - 1).collect();
    short.truncate(short.trim_end().len());
    short.push('…');
    Some(short)
}

/// Drops terminal escape sequences: CSI (`ESC [ ... final`), OSC
/// (`ESC ] ... BEL` or `ESC ] ... ESC \\`) and two-character ones.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Tabs become spaces; control, bidi and zero-width characters go.
fn clean(line: &str) -> String {
    line.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    c,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
        .collect::<String>()
        .trim()
        .to_string()
}
