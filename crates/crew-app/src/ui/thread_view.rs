//! A thread: its header, messages, live run cards and the composer.

use std::collections::HashSet;
use std::ops::Range;

use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ApprovalId, ChannelId, ThreadId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::{
    ApprovalKind, ApprovalStatus, Author, StoredApproval, StoredMessage, StoredRun,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{clock, elapsed, monogram, pulsing, runtime_badge, runtime_chip};
use super::markdown_view::{DocKey, render_doc};
use super::run_card::{decided_asks, summarize, summarize_asked};
use super::thread_settings::policy_name;
use super::workspace::Workspace;
use crate::fonts::{mono_font, ui_font};
use crate::markdown::mention_spans;
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
        let policy = state
            .thread_handoff(thread)
            .0
            .or(state.channel_handoff(t.channel).0);
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
                // An answered permission ask tags its run's step instead,
                // and an ask or stopped call no longer needed shows nowhere.
                let answered = match approval.kind {
                    ApprovalKind::Permission { .. } => approval.status != ApprovalStatus::Pending,
                    ApprovalKind::Blocked { .. } => approval.status == ApprovalStatus::Cancelled,
                    ApprovalKind::Handoff { .. } => false,
                };
                if !answered && !under_run(approval) && carded.insert(approval.id) {
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
                let runtime = agent.map(|a| (runtime_badge(&a.spec), runtime_chip(&a.spec)));
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
                            .when_some(runtime, |row, (badge, text)| {
                                row.child(
                                    badge
                                        .id(format!("message-chip-{id}"))
                                        .aria_label(text)
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
                            .map(|body| {
                                if matches!(message.author, Author::Agent(_)) {
                                    let doc = self.docs.get(DocKey::Message(id), &message.body);
                                    body.child(render_doc(&doc, names, &format!("md-{id}")))
                                } else {
                                    body.child(styled_body(&message.body, names))
                                }
                            })
                            .test_support(),
                    ),
            )
            .test_support()
            .into_any_element()
    }

    /// An agent at work: its avatar, ringed in blue and pulsing while
    /// it works, and its run card.
    fn render_live_run(&self, run: &StoredRun, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let state = self.state();
        let agent = state.agent(run.agent);
        let name = agent.map_or_else(|| "Agent".to_string(), |a| a.spec.name.clone());
        let runtime = agent.map(|a| (runtime_badge(&a.spec), runtime_chip(&a.spec)));
        let asks = decided_asks(&state.thread_approvals(run.thread), run.id);
        let summary = summarize_asked(state.events(run.thread), run.id, &asks);
        let id = run.id;
        div()
            .id(format!("live-{id}"))
            .flex()
            .gap(px(12.))
            .px(px(24.))
            .py(px(8.))
            .bg(color("surface-100"))
            .child(live_avatar(
                &name,
                (run.status == RunStatus::Working).then(|| format!("live-ring-{id}")),
            ))
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
                            .when_some(runtime, |row, (badge, text)| {
                                row.child(
                                    badge
                                        .id(format!("live-chip-{id}"))
                                        .aria_label(text)
                                        .test_support(),
                                )
                            }),
                    )
                    .child(self.render_live_card(run, &summary, cx)),
            )
            .test_support()
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

/// Size of a message avatar.
const AVATAR: f32 = 32.;
/// Space between an avatar and its ring, and the ring's width.
const RING_GAP: f32 = 2.;
const RING_WIDTH: f32 = 2.;

/// A 32 px monogram in a slot of its own size, so it lines up with every
/// other avatar. With `ring`, a pulsing blue ring of that id hugs it,
/// drawn outside the slot so the row doesn't grow or stretch it.
fn live_avatar(name: &str, ring: Option<String>) -> impl IntoElement {
    let out = RING_GAP + RING_WIDTH;
    div()
        .relative()
        .flex_none()
        .self_start()
        .size(px(AVATAR))
        .child(monogram(name, AVATAR))
        .when_some(ring, |slot, ring| {
            slot.child(pulsing(
                SharedString::from(format!("{ring}-pulse")),
                div()
                    .id(SharedString::from(ring))
                    .absolute()
                    .top(px(-out))
                    .left(px(-out))
                    .size(px(AVATAR + 2. * out))
                    // The monogram's 6 px corner plus the gap, at the
                    // ring's inner edge.
                    .rounded(px(6. + RING_GAP + RING_WIDTH))
                    .border_2()
                    .border_color(color("accent-blue"))
                    .test_support(),
            ))
        })
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

#[cfg(test)]
mod tests {
    use super::{Span, body_spans, code_spans};
    use crate::markdown::mention_spans;

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
