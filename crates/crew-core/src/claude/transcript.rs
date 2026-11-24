//! Parsing of the session files Claude Code writes under
//! `~/.claude/projects`, for a read-only view of terminal sessions.
//!
//! Unlike `claude -p` output, these files have no `init` or `result` line,
//! carry the typed prompt as a plain string, put one content block on each
//! assistant line and hold many line types that are not part of the
//! conversation. Those give nothing.

use std::collections::VecDeque;

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
    /// The line is a prompt typed while the agent was busy. Claude Code
    /// may write it again as a plain prompt once it is sent; [`Prompts`]
    /// drops that copy.
    pub queued: bool,
}

/// How many queued prompts are remembered for [`Prompts`].
const QUEUED_KEPT: usize = 16;

/// How long, in milliseconds, a queued prompt waits for Claude Code to
/// write it again as sent.
const QUEUED_FOR: i64 = 2 * 60 * 1000;

/// Reads a session's lines in order, showing a queued prompt once: when
/// it is queued, not again when Claude Code sends it. Each queued prompt
/// hides at most one later copy, written within two minutes.
#[derive(Debug, Clone, Default)]
pub struct Prompts {
    /// Queued text and when it was queued, if the line said.
    queued: VecDeque<(String, Option<i64>)>,
}

impl Prompts {
    /// The items of `line` to show.
    pub fn take(&mut self, line: ParsedLine) -> Vec<TranscriptItem> {
        let now = line.info.timestamp;
        if let Some(now) = now {
            self.queued
                .retain(|(_, at)| at.is_none_or(|at| now - at <= QUEUED_FOR));
        }
        let mut items = line.items;
        if line.queued {
            for item in &items {
                if let TranscriptItem::UserPrompt(text) = item {
                    if self.queued.len() == QUEUED_KEPT {
                        self.queued.pop_front();
                    }
                    self.queued.push_back((text.clone(), now));
                }
            }
            return items;
        }
        items.retain(|item| match item {
            TranscriptItem::UserPrompt(text) => {
                match self
                    .queued
                    .iter()
                    .position(|(q, _)| q.trim() == text.trim())
                {
                    Some(at) => {
                        self.queued.remove(at);
                        false
                    }
                    None => true,
                }
            }
            TranscriptItem::Event(_) => true,
        });
        items
    }
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
    let queued = is_queued(&v);
    ParsedLine {
        info: info(&v),
        items: if queued { queued_prompt(&v) } else { items(&v) },
        queued,
    }
}

fn is_queued(v: &Value) -> bool {
    str_at(v, "type") == "attachment"
        && v.pointer("/attachment/type").and_then(Value::as_str) == Some("queued_command")
}

/// A prompt typed while the agent was busy. Its `origin` is not trusted
/// to hide it: typed input is never hidden on a guess.
fn queued_prompt(v: &Value) -> Vec<TranscriptItem> {
    let a = &v["attachment"];
    if flag(v, "isMeta") || flag(a, "isMeta") {
        return vec![];
    }
    a.get("prompt")
        .and_then(Value::as_str)
        .and_then(prompt)
        .into_iter()
        .collect()
}

/// Words in an `origin` kind or `promptSource` that say a program, not the
/// person, wrote the line.
const NOT_TYPED: &[&str] = &[
    "agent",
    "background",
    "channel",
    "coordinator",
    "cron",
    "hook",
    "monitor",
    "notification",
    "peer",
    "schedul",
    "system",
    "task",
    "teammate",
];

/// Whether a line's `origin` and `promptSource`, if any, say the person
/// typed it. Claude Code leaves `origin` out for what was typed; one that
/// names who sent it (`from`, `producer`, ...) is never the person.
fn human(v: &Value) -> bool {
    let named = |text: &str| {
        let text = text.to_ascii_lowercase();
        NOT_TYPED.iter().any(|w| text.contains(w))
    };
    if let Some(origin) = v.get("origin").filter(|o| !o.is_null()) {
        let sender = ["from", "producer", "body", "senderTaskId", "runId"]
            .iter()
            .any(|k| origin.get(k).is_some());
        let kind = origin.get("kind").and_then(Value::as_str).unwrap_or("");
        if sender || named(kind) || !origin.is_object() {
            return false;
        }
    }
    !v.get("promptSource")
        .and_then(Value::as_str)
        .is_some_and(named)
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
    // So are lines shown only in the full transcript view.
    if flag(v, "isMeta")
        || flag(v, "isSidechain")
        || flag(v, "isCompactSummary")
        || flag(v, "isVisibleInTranscriptOnly")
    {
        return vec![];
    }
    match str_at(v, "type") {
        "user" => {
            let typed = human(v);
            match v.pointer("/message/content") {
                Some(Value::String(text)) if typed => prompt(text).into_iter().collect(),
                Some(Value::String(_)) => vec![],
                _ => {
                    let blocks = blocks(v);
                    // Text next to tool results was added by Claude Code,
                    // not typed.
                    let results = blocks.iter().any(|b| str_at(b, "type") == "tool_result");
                    blocks
                        .iter()
                        .filter_map(|b| match str_at(b, "type") {
                            "text" if typed && !results => prompt(str_at(b, "text")),
                            "text" => None,
                            _ => tool_result(b).map(TranscriptItem::Event),
                        })
                        .collect()
                }
            }
        }
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
    (!text.trim().is_empty() && !is_wrapped(text))
        .then(|| TranscriptItem::UserPrompt(text.to_string()))
}

/// Text Claude Code wraps in a tag of its own, such as
/// `<system-reminder>`, `<task-notification>` or `<command-name>`: what it
/// added, not what the person typed. A shell command typed with `!`
/// (`<bash-input>`) is the person's.
fn is_wrapped(text: &str) -> bool {
    let text = text.trim_start();
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let name_len = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    let name = &rest[..name_len];
    let starts_lower = name.starts_with(|c: char| c.is_ascii_lowercase());
    // `<local-command-caveat>` and friends, or any `<tag>`.
    starts_lower && rest[name_len..].starts_with('>') && name != "bash-input"
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
/// control characters, at most 80 characters. `None` for text Claude Code
/// wrapped in a tag, a shell command or a prompt with no text.
pub fn title_from(prompt: &str) -> Option<String> {
    if is_wrapped(prompt) || prompt.trim_start().starts_with("<bash-") {
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

/// Tabs become spaces; hidden characters go; the ends are trimmed.
fn clean(line: &str) -> String {
    crate::text::clean_line(line).trim().to_string()
}
