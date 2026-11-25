//! Small pieces several views draw: monograms, chips, status words and
//! times.

use std::f32::consts::TAU;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crew_core::agent::{AgentSpec, Runtime};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::model::ThreadStatus;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

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

/// The Claude mark, from lobe-icons (MIT, see THIRD_PARTY_NOTICES.md).
const CLAUDE_SVG: &[u8] = include_bytes!("../../assets/icons/claude.svg");
/// The OpenAI mark, from lobe-icons (MIT, see THIRD_PARTY_NOTICES.md).
const OPENAI_SVG: &[u8] = include_bytes!("../../assets/icons/openai.svg");

/// The mark of the company behind a runtime's CLI, `size` px, tinted in
/// its colour. Queryable as `provider-claude` or `provider-openai`.
pub fn provider_icon(runtime: &Runtime, size: f32) -> impl IntoElement + use<> {
    let (id, label, svg, token) = match runtime {
        Runtime::ClaudeCode => ("provider-claude", "Claude mark", CLAUDE_SVG, "brand-claude"),
        Runtime::Codex => ("provider-openai", "OpenAI mark", OPENAI_SVG, "text-primary"),
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(ui_px(size))
        .aria_label(label)
        .child(
            Icon::default()
                .data(svg)
                .size(ui_px(size))
                .text_color(color(token)),
        )
        .test_support()
}

/// A runtime chip with its provider's mark before the text, which reads
/// as [`runtime_chip`].
pub fn runtime_badge(spec: &AgentSpec) -> Div {
    runtime_badge_for(&spec.runtime, runtime_chip(spec))
}

/// Like [`runtime_badge`] for a chip text already made.
pub fn runtime_badge_for(runtime: &Runtime, text: String) -> Div {
    chip_frame()
        .flex()
        .items_center()
        .gap(ui_px(4.))
        .child(provider_icon(runtime, 12.))
        .child(text)
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
/// carriage returns become spaces, and other control, bidi and
/// zero-width characters are dropped, so they can't hide or reorder what
/// you read.
pub fn clean_line(text: &str) -> String {
    crew_core::text::clean_line(text)
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

/// How long ago `then` was at `now` (both Unix ms): `now`, `5m ago`,
/// `3h ago` or `2d ago`.
pub fn ago(then: i64, now: i64) -> String {
    let mins = (now - then).max(0) / 60_000;
    match mins {
        0 => "now".into(),
        m if m < 60 => format!("{m}m ago"),
        m if m < 24 * 60 => format!("{}h ago", m / 60),
        m => format!("{}d ago", m / (24 * 60)),
    }
}

/// `text` cut to `max` characters by dropping its start, for paths whose
/// end says the most: `…/crates/app`.
pub fn cut_start(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let tail: String = text.chars().skip(n + 1 - max).collect();
    format!("…{tail}")
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

thread_local! {
    /// The letters of each agent's monogram, by name, as last worked out.
    static LETTERS: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::default();
}

/// Each name's monogram letters, names in the order the agents were made:
/// the first letter, or the first two once an earlier name took that
/// letter, as Scout then Sentinel read `S` and `Se`.
pub fn monogram_letters(names: &[&str]) -> Vec<String> {
    let first = |name: &str| name.chars().next().map(|c| c.to_lowercase().to_string());
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mut chars = name.chars();
            let one: String = chars
                .next()
                .map(|c| c.to_uppercase().collect())
                .unwrap_or_default();
            let clash = names[..i].iter().any(|n| first(n) == first(name));
            match chars.next() {
                Some(second) if clash => format!("{one}{}", second.to_lowercase()),
                _ => one,
            }
        })
        .collect()
}

/// Sets the agents' names, oldest first, that monograms tell apart.
pub fn set_agent_names(names: &[&str]) {
    let letters = monogram_letters(names);
    LETTERS.with(|l| {
        let mut l = l.borrow_mut();
        l.clear();
        l.extend(names.iter().map(|n| n.to_string()).zip(letters));
    });
}

/// A grey square with the name's letters: the first, or two when another
/// agent's name starts the same.
pub fn monogram(name: &str, size: f32) -> Div {
    let letter: String = LETTERS
        .with(|l| l.borrow().get(name).cloned())
        .unwrap_or_else(|| {
            name.chars()
                .next()
                .map(|c| c.to_uppercase().collect())
                .unwrap_or_default()
        });
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(ui_px(size))
        .rounded(ui_px(6.))
        .bg(color("surface-300"))
        .border_1()
        .border_color(color("border-200"))
        .text_size(scale::text((size * 0.45).round().max(10.)))
        .font_weight(weight(FontWeight::SEMIBOLD))
        .text_color(color("text-primary"))
        .child(letter)
}

/// Mono text on a chip, for runtimes and paths.
pub fn chip(text: impl IntoElement) -> Div {
    chip_frame().child(text)
}

fn chip_frame() -> Div {
    div()
        .flex_none()
        .px(ui_px(6.))
        .rounded(ui_px(4.))
        .bg(color("surface-300"))
        .font_family(code_family())
        .text_size(scale::mono(11.))
        .line_height(scale::mono(16.))
        .text_color(color("text-secondary"))
}

/// How long the band takes to cross a title being generated.
const TITLE_SWEEP: Duration = Duration::from_millis(1200);
/// How long a generated title takes to fade in.
const TITLE_FADE: Duration = Duration::from_millis(150);
/// The light band of a title being generated.
const TITLE_BAND: u32 = 0xcfcfcf;

/// A thread's title, inside an element that sets its size and cuts it.
/// While a title is generated, the first line shows with a light band
/// sweeping left to right (static under Reduce motion); the element gives
/// it [`generating_title`]'s weight and colour. A title that has
/// `just_landed` fades in.
pub fn thread_title_text(
    id: impl Into<SharedString>,
    title: &str,
    generating: bool,
    just_landed: bool,
) -> AnyElement {
    let id: SharedString = id.into();
    if generating {
        return gpui_kit::component::shimmer::ShimmerText::new(title.to_string())
            .id(SharedString::from(format!("{id}-shimmer")))
            .duration(TITLE_SWEEP)
            .highlight_color(rgb(TITLE_BAND))
            .min_w_0()
            .truncate()
            .into_any_element();
    }
    let text = div().min_w_0().truncate().child(title.to_string());
    if just_landed {
        return text
            .with_animation(
                ElementId::Name(format!("{id}-fade-{title}").into()),
                Animation::new(TITLE_FADE),
                |el, t| el.opacity(t),
            )
            .into_any_element();
    }
    text.into_any_element()
}

/// A title being generated reads in medium tertiary grey.
pub fn generating_title<E: Styled>(el: E) -> E {
    el.font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-tertiary"))
}

/// The boards' small switch, 28 by 16. Off, its knob is grey on the
/// border colour.
pub fn switch(on: bool) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .when(on, |t| t.justify_end())
        .w(ui_px(28.))
        .h(ui_px(16.))
        .p(ui_px(2.))
        .rounded_full()
        .bg(color(if on { "text-primary" } else { "border-200" }))
        .child(div().size(ui_px(12.)).rounded_full().bg(color(if on {
            "bg-100"
        } else {
            "text-tertiary"
        })))
}

/// A small round dot in the colour token `token`.
pub fn dot(token: &str) -> Div {
    div()
        .flex_none()
        .size(ui_px(6.))
        .rounded_full()
        .bg(color(token))
}

/// One turn of a spinner.
const SPIN: Duration = Duration::from_millis(800);
/// How long a changed digit takes to roll in.
const ROLL: Duration = Duration::from_millis(220);
/// One pulse of the live avatar ring.
const PULSE: Duration = Duration::from_millis(1600);

/// A loader that turns once every 0.8 s, `size` px in the colour token
/// `token`. `id` keeps its turn going across redraws.
pub fn spinner(id: impl Into<ElementId>, size: f32, token: &str) -> impl IntoElement {
    Icon::new(IconName::LoaderCircle)
        .size(ui_px(size))
        .text_color(color(token))
        .with_animation(id, Animation::new(SPIN).repeat(), |icon, turn| {
            icon.transform(Transformation::rotate(percentage(turn)))
        })
}

/// The live avatar ring's opacity at `phase` (0 to 1) of a pulse: 1.0 at
/// the ends, 0.4 halfway.
pub fn pulse_opacity(phase: f32) -> f32 {
    0.7 + 0.3 * (phase * TAU).cos()
}

/// `element` fading from full to 0.4 and back every 1.6 s.
pub fn pulsing<E: Styled + IntoElement + 'static>(
    id: impl Into<ElementId>,
    element: E,
) -> impl IntoElement {
    element.with_animation(id, Animation::new(PULSE).repeat(), |el, phase| {
        el.opacity(pulse_opacity(phase))
    })
}

/// Which characters of `cur` differ from `prev`, by position. When the
/// length changed, as from `59s` to `1m 0s`, every one did.
pub fn changed_positions(prev: &str, cur: &str) -> Vec<usize> {
    let (a, b): (Vec<char>, Vec<char>) = (prev.chars().collect(), cur.chars().collect());
    if a.len() != b.len() {
        return (0..b.len()).collect();
    }
    (0..b.len()).filter(|&i| a[i] != b[i]).collect()
}

/// Height of one line of rolling text.
pub const ROLL_LINE: f32 = 16.;

/// `cur` drawn so each character that changed from `prev` rolls up into
/// place like an odometer. Unchanged characters stay still. When the
/// length changed the whole text rolls as one.
pub fn rolling_text(id: impl Into<ElementId>, prev: &str, cur: &str) -> Stateful<Div> {
    let row = div()
        .id(id)
        .flex()
        .flex_none()
        .h(scale::line(ROLL_LINE))
        .line_height(scale::line(ROLL_LINE))
        .overflow_hidden();
    if prev.chars().count() != cur.chars().count() {
        return row.child(rolling_slot(0, prev.to_string(), cur.to_string(), cur));
    }
    let changed = changed_positions(prev, cur);
    let prev: Vec<char> = prev.chars().collect();
    row.children(cur.chars().enumerate().map(|(i, c)| {
        if changed.contains(&i) {
            rolling_slot(i, prev[i].to_string(), c.to_string(), cur).into_any_element()
        } else {
            div()
                .flex_none()
                .whitespace_nowrap()
                .child(c.to_string())
                .into_any_element()
        }
    }))
}

/// One rolling place: `from` above `to`, moving up one line. The
/// animation is named after the whole text `cur` so each change starts a
/// fresh roll.
fn rolling_slot(ix: usize, from: String, to: String, cur: &str) -> impl IntoElement {
    div()
        .flex_none()
        .h(scale::line(ROLL_LINE))
        .overflow_hidden()
        .child(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(scale::line(ROLL_LINE))
                        .whitespace_nowrap()
                        .child(from),
                )
                .child(
                    div()
                        .h(scale::line(ROLL_LINE))
                        .whitespace_nowrap()
                        .child(to),
                )
                .with_animation(
                    SharedString::from(format!("roll-{ix}-{cur}")),
                    Animation::new(ROLL).with_easing(ease_out_cubic),
                    |column, t| column.mt(-scale::line(ROLL_LINE) * t),
                ),
        )
}

fn ease_out_cubic(t: f32) -> f32 {
    1. - (1. - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::{
        ago, changed_positions, codex_short, cut_start, elapsed, monogram_letters, pulse_opacity,
        runtime_chip, short_model,
    };
    use crate::ui::settings::agents::blank_spec;
    use crew_core::agent::{AgentSpec, Runtime};

    #[test]
    fn monograms_take_two_letters_once_an_initial_is_taken() {
        assert_eq!(
            monogram_letters(&["Forge", "Scout", "Triage", "Sentinel", "sage", "X"]),
            ["F", "S", "T", "Se", "Sa", "X"]
        );
    }

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
    fn ago_reads_in_the_largest_unit() {
        let min = 60_000;
        assert_eq!(ago(1000, 30_000), "now");
        assert_eq!(ago(0, 5 * min), "5m ago");
        assert_eq!(ago(0, 3 * 60 * min + 5), "3h ago");
        assert_eq!(ago(0, 49 * 60 * min), "2d ago");
        assert_eq!(ago(10, 0), "now", "a clock ahead");
    }

    #[test]
    fn cut_start_keeps_the_end() {
        assert_eq!(cut_start("/work/demo", 20), "/work/demo");
        assert_eq!(cut_start("/work/demo/crates/app", 10), "…rates/app");
        assert_eq!(cut_start("/work/demo/crates/app", 10).chars().count(), 10);
    }

    #[test]
    fn elapsed_reads_like_a_stopwatch() {
        assert_eq!(elapsed(12_400), "12s");
        assert_eq!(elapsed(252_000), "4m 12s");
        assert_eq!(elapsed(3_780_000), "1h 3m");
        assert_eq!(elapsed(-5), "0s");
    }

    #[test]
    fn only_changed_characters_roll() {
        assert_eq!(changed_positions("22s", "23s"), [1]);
        assert_eq!(changed_positions("29s", "30s"), [0, 1]);
        assert_eq!(changed_positions("4m 12s", "4m 13s"), [4]);
        assert_eq!(changed_positions("4m 59s", "5m 0s"), [0, 1, 2, 3, 4]);
        assert_eq!(changed_positions("23s", "23s"), Vec::<usize>::new());
        assert_eq!(changed_positions("59s", "1m 0s"), [0, 1, 2, 3, 4], "longer");
        assert_eq!(changed_positions("1m 0s", "9s"), [0, 1], "shorter");
        assert_eq!(changed_positions("", "0s"), [0, 1]);
        assert_eq!(
            changed_positions(&elapsed(9_000), &elapsed(10_000)),
            [0, 1, 2]
        );
    }

    #[test]
    fn the_ring_pulses_between_full_and_forty_percent() {
        assert!((pulse_opacity(0.) - 1.).abs() < 1e-6);
        assert!((pulse_opacity(0.5) - 0.4).abs() < 1e-6);
        assert!((pulse_opacity(1.) - 1.).abs() < 1e-6);
        assert!((pulse_opacity(0.25) - 0.7).abs() < 1e-6);
    }
}

/// Adds a probe that records the text size and weight `el` draws with
/// under `key`, in tests only, so they can check what the appearance
/// settings did.
pub fn with_font_probe<E: ParentElement>(el: E, key: impl FnOnce() -> String) -> E {
    #[cfg(test)]
    {
        let key = key();
        let mut el = el;
        el.extend([canvas(
            move |_, window, _| font_probe::record(key, window),
            |_, _, _, _| {},
        )
        .absolute()
        .into_any_element()]);
        el
    }
    #[cfg(not(test))]
    {
        let _ = key;
        el
    }
}

#[cfg(test)]
pub mod font_probe {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use gpui_kit::{FontWeight, Pixels, Window};

    thread_local! {
        static DRAWN: RefCell<HashMap<String, (Pixels, FontWeight)>> = RefCell::new(HashMap::new());
    }

    pub(super) fn record(key: String, window: &Window) {
        let style = window.text_style();
        let size = style.font_size.to_pixels(window.rem_size());
        DRAWN.with(|s| s.borrow_mut().insert(key, (size, style.font_weight)));
    }

    /// The text size the element probed as `key` last drew at.
    pub fn size_of(key: &str) -> Option<Pixels> {
        DRAWN.with(|s| s.borrow().get(key).map(|d| d.0))
    }

    /// The weight the element probed as `key` last drew its text with.
    pub fn weight_of(key: &str) -> Option<FontWeight> {
        DRAWN.with(|s| s.borrow().get(key).map(|d| d.1))
    }
}
