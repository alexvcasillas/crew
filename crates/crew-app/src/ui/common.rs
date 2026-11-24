//! Small pieces several views draw: monograms, chips, status words and
//! times.

use std::time::{SystemTime, UNIX_EPOCH};

use crew_core::agent::{AgentSpec, Runtime};
use gpui_kit::*;

use crate::fonts::mono_font;
use crate::model::ThreadStatus;
use crate::theme::color;

/// Now, in Unix milliseconds like crewd's timestamps.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `claude · haiku` or `codex · luna`: the runtime and the short model
/// name.
pub fn runtime_chip(spec: &AgentSpec) -> String {
    match spec.runtime {
        Runtime::ClaudeCode => format!("claude · {}", short_model(&spec.model)),
        Runtime::Codex => format!("codex · {}", codex_short(&spec.model, None)),
    }
}

/// The Codex model families a chip names on their own.
const CODEX_FAMILIES: [&str; 4] = ["luna", "sol", "terra", "astra"];

/// A Codex model as a chip names it: the family its label (or else its
/// id) ends with, such as `luna` for `gpt-6-luna`, else the id. An empty
/// id is Codex's own default.
pub fn codex_short(id: &str, label: Option<&str>) -> String {
    if id.trim().is_empty() {
        return "default".into();
    }
    let family = |text: &str| {
        let last = text.rsplit([' ', '-']).next()?.to_lowercase();
        CODEX_FAMILIES.contains(&last.as_str()).then_some(last)
    };
    label
        .and_then(family)
        .or_else(|| family(id))
        .unwrap_or_else(|| id.to_string())
}

/// `claude-haiku-4-5` reads as `haiku`. Other names stay as they are.
pub fn short_model(model: &str) -> &str {
    let rest = model.strip_prefix("claude-").unwrap_or(model);
    match rest.split('-').next() {
        Some(family) if !family.is_empty() && family != rest => family,
        _ => rest,
    }
}

/// One line of text an agent or CLI wrote, safe to draw: tabs and
/// carriage returns become spaces, and other control and bidi characters
/// are dropped, so they can't hide or reorder what you read.
pub fn clean_line(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\t' | '\r' => Some(' '),
            c if c.is_control() => None,
            '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => None,
            c => Some(c),
        })
        .collect()
}

/// Like [`clean_line`] for each line of `text`.
pub fn clean_lines(text: &str) -> String {
    text.lines().map(clean_line).collect::<Vec<_>>().join("\n")
}

/// `4m 12s`, `12s` or `1h 3m`.
pub fn elapsed(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// The local wall-clock time of `ms`, such as `10:05`.
pub fn clock(ms: i64) -> String {
    use chrono::{Local, TimeZone as _};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or_else(String::new, |t| t.format("%H:%M").to_string())
}

/// The word and colour token for a status.
pub fn status_word(status: ThreadStatus) -> (&'static str, &'static str) {
    match status {
        ThreadStatus::Working => ("Working", "accent-blue"),
        ThreadStatus::NeedsYou => ("Needs you", "warning"),
        ThreadStatus::Idle => ("Idle", "text-tertiary"),
        ThreadStatus::Queued => ("Queued", "text-tertiary"),
    }
}

/// A grey square with the name's first letter.
pub fn monogram(name: &str, size: f32) -> Div {
    let letter: String = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(6.))
        .bg(color("surface-300"))
        .border_1()
        .border_color(color("border-200"))
        .text_size(px((size * 0.45).round().max(10.)))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color("text-secondary"))
        .child(letter)
}

/// Mono text on a chip, for runtimes and paths.
pub fn chip(text: impl IntoElement) -> Div {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(4.))
        .bg(color("surface-300"))
        .border_1()
        .border_color(color("border-100"))
        .font_family(mono_font())
        .text_size(px(11.))
        .text_color(color("text-secondary"))
        .child(text)
}

/// A small round dot in the colour token `token`.
pub fn dot(token: &str) -> Div {
    div()
        .flex_none()
        .size(px(6.))
        .rounded_full()
        .bg(color(token))
}

#[cfg(test)]
mod tests {
    use super::{codex_short, elapsed, runtime_chip, short_model};
    use crate::ui::settings::agents::blank_spec;
    use crew_core::agent::{AgentSpec, Runtime};

    #[test]
    fn model_names_read_short() {
        assert_eq!(short_model("claude-haiku-4-5"), "haiku");
        assert_eq!(short_model("claude-opus-4-1"), "opus");
        assert_eq!(short_model("sonnet"), "sonnet");
        assert_eq!(short_model("claude-sonnet"), "sonnet");
    }

    #[test]
    fn codex_models_read_as_their_family() {
        assert_eq!(codex_short("gpt-6-luna", Some("GPT-6-Luna")), "luna");
        assert_eq!(codex_short("gpt-6.1-terra", Some("GPT-6.1 Terra")), "terra");
        assert_eq!(codex_short("gpt-6.1-sol", None), "sol");
        assert_eq!(codex_short("gpt-a", Some("GPT A")), "gpt-a");
        assert_eq!(codex_short("", None), "default");
        let codex = AgentSpec {
            runtime: Runtime::Codex,
            model: "gpt-6-luna".into(),
            ..blank_spec()
        };
        assert_eq!(runtime_chip(&codex), "codex · luna");
        let claude = AgentSpec {
            model: "claude-opus-4-1".into(),
            ..blank_spec()
        };
        assert_eq!(runtime_chip(&claude), "claude · opus");
    }

    #[test]
    fn elapsed_reads_like_a_stopwatch() {
        assert_eq!(elapsed(12_400), "12s");
        assert_eq!(elapsed(252_000), "4m 12s");
        assert_eq!(elapsed(3_780_000), "1h 3m");
        assert_eq!(elapsed(-5), "0s");
    }
}
