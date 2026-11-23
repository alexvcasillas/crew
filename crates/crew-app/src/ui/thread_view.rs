//! A thread: its header, messages, live run cards and the composer.

use std::collections::HashSet;
use std::ops::Range;

use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ApprovalId, ChannelId, ThreadId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::{ApprovalStatus, Author, StoredApproval, StoredMessage, StoredRun};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{chip, clock, elapsed, monogram, runtime_chip};
use super::run_card::summarize;
use super::thread_settings::policy_name;
use super::workspace::Workspace;
use crate::fonts::{mono_font, ui_font};
use crate::theme::color;

impl Workspace {
    pub(super) fn render_thread(
        &self,
        thread: ThreadId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let state = self.state();
        let Some(t) = state.thread(thread) else {
            return div().into_any_element();
        };
        let channel = state.channel(t.channel);
        let channel_name = channel.map(|c| c.name.clone()).unwrap_or_default();
        let policy = t
            .handoff_policy
            .or_else(|| channel.and_then(|c| c.handoff_policy));
        let events = state.events(thread);
        let names: Vec<String> = state.agents().iter().map(|a| a.spec.name.clone()).collect();

        let mut items: Vec<AnyElement> = Vec::new();
        let live = state.live_runs(thread);
        let pending = state.pending_approvals(thread);
        // A pending ask of a live run stands right under the run's card.
        let under_run = |approval: &StoredApproval| {
            approval.status == ApprovalStatus::Pending && live.iter().any(|r| r.id == approval.run)
        };
        // Any other approval's card stands where it was asked; later notes
        // about it fold into the card's line.
        let mut carded: HashSet<ApprovalId> = HashSet::new();
        for message in state.messages(thread) {
            if let Some(approval) = message.approval.and_then(|a| state.approval(a)) {
                if !under_run(approval) && carded.insert(approval.id) {
                    items.push(self.render_approval(approval, cx));
                }
                continue;
            }
            items.push(self.render_message(message, &names).into_any_element());
            if let (Author::Agent(_), Some(run)) = (&message.author, message.run)
                && state.run(run).is_some_and(|r| r.status == RunStatus::Done)
            {
                let summary = summarize(events, run);
                if !summary.files.is_empty() {
                    items.push(
                        indented(self.render_done_card(run, &summary, cx)).into_any_element(),
                    );
                }
            }
        }
        for approval in &pending {
            if !carded.contains(&approval.id) && !under_run(approval) {
                items.push(self.render_approval(approval, cx));
            }
        }
        for run in &live {
            items.push(self.render_live_run(run, cx).into_any_element());
            for approval in pending.iter().filter(|a| a.run == run.id) {
                items.push(self.render_approval(approval, cx));
            }
        }

        div()
            .id("thread-view")
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(12.))
                    .h(px(44.))
                    .px(px(24.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .id("thread-title")
                                    .truncate()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .aria_label(t.title.clone())
                                    .child(t.title.clone())
                                    .test_support(),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(color("text-tertiary"))
                                    .child(format!("Thread in #{channel_name}")),
                            ),
                    )
                    .child(self.render_participants(thread))
                    .child({
                        let (label, icon) = if self.inspector_shown {
                            ("Hide activity", IconName::PanelRightClose)
                        } else {
                            ("Show activity", IconName::PanelRightOpen)
                        };
                        Button::new("toggle-activity")
                            .ghost()
                            .small()
                            .icon(icon)
                            .tooltip(label)
                            .accessibility_label(label)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector(cx)))
                    })
                    .child({
                        let handoffs = format!(
                            "Handoffs: {}",
                            policy_name(policy.unwrap_or(HandoffPolicy::AskFirst))
                        );
                        div()
                            .id("thread-handoffs")
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(color("text-secondary"))
                            .aria_label(handoffs.clone())
                            .child(handoffs)
                            .test_support()
                    }),
            )
            .child(
                div()
                    .id("thread-messages")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.nav.scroll)
                    .py(px(12.))
                    .gap(px(12.))
                    .children(items)
                    .test_support(),
            )
            .child(self.render_composer_area())
            .test_support()
            .into_any_element()
    }

    /// The main area while you write the first message of a new thread.
    pub(super) fn render_new_thread(&self, channel: ChannelId) -> impl IntoElement + use<> {
        let name = self
            .state()
            .channel(channel)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        div()
            .id("new-thread-view")
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .flex_col()
                    .justify_center()
                    .h(px(44.))
                    .px(px(24.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .child(
                        div()
                            .id("new-thread-title")
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .aria_label("New thread")
                            .child("New thread")
                            .test_support(),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(color("text-tertiary"))
                            .child(format!("In #{name}")),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(color("text-tertiary"))
                    .child("Say what you need. Mention an agent with @ to start it."),
            )
            .child(self.render_composer_area())
            .test_support()
    }

    fn render_composer_area(&self) -> impl IntoElement + use<> {
        div()
            .flex_none()
            .px(px(24.))
            .pb(px(16.))
            .pt(px(4.))
            .child(self.composer.clone())
    }

    fn render_message(
        &self,
        message: &StoredMessage,
        names: &[String],
    ) -> impl IntoElement + use<> {
        let state = self.state();
        let id = message.id;
        let mut time = clock(message.created_at);
        if let Some(run) = message.run.and_then(|r| state.run(r))
            && let Some(ended) = run.ended_at
        {
            time = format!("{time} · {}", elapsed(ended - run.started_at));
        }
        if message.author == Author::System {
            return div()
                .id(format!("message-{id}"))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(24.))
                .pl(px(24. + 32. + 12.))
                .text_size(px(12.))
                .text_color(color("text-tertiary"))
                .child(
                    Icon::new(IconName::Info)
                        .size(px(12.))
                        .text_color(color("text-tertiary")),
                )
                .child(
                    div()
                        .id(format!("message-body-{id}"))
                        .aria_label(message.body.clone())
                        .child(message.body.clone())
                        .test_support(),
                )
                .child(format!("· {time}"))
                .test_support()
                .into_any_element();
        }
        let (name, avatar, runtime) = match &message.author {
            Author::Agent(agent) => {
                let agent = state.agent(*agent);
                let name =
                    agent.map_or_else(|| "Deleted agent".to_string(), |a| a.spec.name.clone());
                let runtime = agent.map(|a| runtime_chip(&a.spec));
                (name.clone(), monogram(&name, 32.), runtime)
            }
            _ => ("You".to_string(), you_avatar(), None),
        };
        div()
            .id(format!("message-{id}"))
            .flex()
            .gap(px(12.))
            .px(px(24.))
            .py(px(8.))
            .child(avatar)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                            .when_some(runtime, |row, runtime| {
                                row.child(
                                    chip(runtime.clone())
                                        .id(format!("message-chip-{id}"))
                                        .aria_label(runtime)
                                        .test_support(),
                                )
                            })
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(color("text-tertiary"))
                                    .child(time),
                            ),
                    )
                    .child(
                        div()
                            .id(format!("message-body-{id}"))
                            .line_height(px(20.))
                            .aria_label(message.body.clone())
                            .child(styled_body(&message.body, names))
                            .test_support(),
                    ),
            )
            .test_support()
            .into_any_element()
    }

    /// An agent at work: its avatar with a blue ring and its run card.
    fn render_live_run(&self, run: &StoredRun, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let state = self.state();
        let agent = state.agent(run.agent);
        let name = agent.map_or_else(|| "Agent".to_string(), |a| a.spec.name.clone());
        let runtime = agent.map(|a| runtime_chip(&a.spec));
        let summary = summarize(state.events(run.thread), run.id);
        div()
            .id(format!("live-{}", run.id))
            .flex()
            .gap(px(12.))
            .px(px(24.))
            .py(px(8.))
            .bg(color("surface-100"))
            .child(
                div()
                    .flex_none()
                    .p(px(1.))
                    .rounded(px(8.))
                    .border_2()
                    .border_color(color(if run.status == RunStatus::Working {
                        "accent-blue"
                    } else {
                        "border-200"
                    }))
                    .child(monogram(&name, 30.)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
                            .when_some(runtime, |row, runtime| row.child(chip(runtime))),
                    )
                    .child(self.render_live_card(run, &summary, cx)),
            )
    }

    /// Overlapping monograms of the agents in the thread.
    fn render_participants(&self, thread: ThreadId) -> impl IntoElement + use<> {
        let state = self.state();
        let mut agents: Vec<AgentId> = Vec::new();
        let authors = state
            .messages(thread)
            .iter()
            .filter_map(|m| match m.author {
                Author::Agent(a) => Some(a),
                _ => None,
            });
        let live = state.live_runs(thread).into_iter().map(|r| r.agent);
        for agent in authors.chain(live) {
            if !agents.contains(&agent) {
                agents.push(agent);
            }
        }
        div()
            .flex()
            .flex_none()
            .children(agents.into_iter().enumerate().map(|(ix, a)| {
                let name = state
                    .agent(a)
                    .map(|a| a.spec.name.clone())
                    .unwrap_or_default();
                monogram(&name, 22.)
                    .border_color(color("bg-100"))
                    .when(ix > 0, |m| m.ml(px(-6.)))
            }))
    }
}

/// Lines up with a message's text, past the avatar.
pub(super) fn indented(child: impl IntoElement) -> Div {
    div().pl(px(24. + 32. + 12.)).pr(px(24.)).child(child)
}

fn you_avatar() -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(32.))
        .rounded_full()
        .bg(color("border-200"))
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .child("Y")
}

/// How a stretch of a message body is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Span {
    Plain,
    Mention,
    Code,
}

/// The body cut into stretches covering all of it, in order: `code`
/// first, then known `@Agent` mentions outside code.
fn body_spans(body: &str, names: &[String]) -> Vec<(Range<usize>, Span)> {
    let mut marked: Vec<(Range<usize>, Span)> = code_spans(body)
        .into_iter()
        .map(|r| (r, Span::Code))
        .collect();
    for range in mention_spans(body, names) {
        if !marked
            .iter()
            .any(|(r, _)| r.start < range.end && range.start < r.end)
        {
            marked.push((range, Span::Mention));
        }
    }
    marked.sort_by_key(|(r, _)| r.start);
    let mut spans = Vec::new();
    let mut at = 0;
    for (range, span) in marked {
        if at < range.start {
            spans.push((at..range.start, Span::Plain));
        }
        at = range.end;
        spans.push((range, span));
    }
    if at < body.len() {
        spans.push((at..body.len(), Span::Plain));
    }
    spans
}

/// The body with `@Agent` mentions in blue and `code` in mono on a
/// background. Text runs share one size and have no corner radius.
fn styled_body(body: &str, names: &[String]) -> StyledText {
    let runs = body_spans(body, names)
        .into_iter()
        .map(|(range, span)| {
            let (mut font, text, background) = match span {
                Span::Plain => (font(ui_font()), "text-primary", None),
                Span::Mention => (font(ui_font()), "accent-blue", None),
                Span::Code => (
                    font(mono_font()),
                    "text-primary",
                    Some(color("surface-300")),
                ),
            };
            if span == Span::Mention {
                font.weight = FontWeight::MEDIUM;
            }
            TextRun {
                len: range.len(),
                font,
                color: color(text),
                background_color: background,
                underline: None,
                strikethrough: None,
            }
        })
        .collect();
    StyledText::new(body.to_string()).with_runs(runs)
}

/// Byte ranges of `code` spans, backticks included.
fn code_spans(body: &str) -> Vec<Range<usize>> {
    let ticks: Vec<usize> = body.match_indices('`').map(|(i, _)| i).collect();
    ticks
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[open, close]| *open..*close + 1)
        .collect()
}

/// Byte ranges of `@Name` for known agent names.
fn mention_spans(body: &str, names: &[String]) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    for (at, _) in body.match_indices('@') {
        let starts = body[..at]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == '(');
        let word_len = body[at + 1..]
            .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(body.len() - at - 1);
        let word = &body[at + 1..at + 1 + word_len];
        if starts && names.iter().any(|n| n.eq_ignore_ascii_case(word)) {
            spans.push(at..at + 1 + word_len);
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::{Span, body_spans, code_spans, mention_spans};

    #[test]
    fn known_mentions_and_code_are_found() {
        let names = ["Forge".to_string()];
        let body = "@forge read `a.txt`, not @nobody or me@forge";
        assert_eq!(mention_spans(body, &names), vec![0..6_usize]);
        assert_eq!(&body[code_spans(body)[0].clone()], "`a.txt`");
        assert!(code_spans("one ` tick").is_empty());
    }

    #[test]
    fn spans_cover_the_whole_body() {
        let names = ["Forge".to_string()];
        let body = "hi @Forge, see `@Forge x` ok";
        let spans = body_spans(body, &names);
        let kinds: Vec<(&str, Span)> = spans.iter().map(|(r, k)| (&body[r.clone()], *k)).collect();
        assert_eq!(
            kinds,
            [
                ("hi ", Span::Plain),
                ("@Forge", Span::Mention),
                (", see ", Span::Plain),
                ("`@Forge x`", Span::Code),
                (" ok", Span::Plain),
            ]
        );
        assert!(body_spans("", &names).is_empty());
    }
}
