//! The popover beside a hovered row of the threads sidebar: the thread
//! at a glance, with Open and Review changes.

use std::time::Duration;

use crew_core::event::RunStatus;
use crew_core::ids::ThreadId;
use crew_core::store::StoredApproval;
use crew_proto::WorktreeState;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::channel_view::CardStatus;
use super::super::common::{ago, elapsed, now_ms, runtime_chip};
use super::super::workspace::{Place, Workspace};
use super::threads::{plural, span, wants};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// How long the popover stays once the pointer leaves its row.
const POPOVER_LINGER: Duration = Duration::from_millis(150);
/// How far above its row's top the popover starts, from the board: level
/// with the group heading over the first row.
const POPOVER_ABOVE: f32 = 27.;
/// The popover's width, from the board.
const POPOVER_WIDTH: f32 = 300.;

impl Workspace {
    pub(super) fn thread_row_hovered(
        &mut self,
        thread: ThreadId,
        ix: usize,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let t = &mut self.sidebar.threads;
        if hovered {
            t.linger = None;
            if t.hovered != Some((thread, ix)) {
                t.hovered = Some((thread, ix));
                cx.notify();
            }
            return;
        }
        if t.hovered.is_some_and(|(h, _)| h == thread) {
            self.close_popover_later(cx);
        }
    }

    /// Closes the popover after [`POPOVER_LINGER`], unless the pointer
    /// went into it or onto another row.
    pub(super) fn close_popover_later(&mut self, cx: &mut Context<Self>) {
        let shown = self.sidebar.threads.hovered;
        self.sidebar.threads.linger = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(POPOVER_LINGER).await;
            this.update(cx, |this, cx| {
                let t = &mut this.sidebar.threads;
                t.linger = None;
                if !t.popover_hovered && t.hovered == shown {
                    t.hovered = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The hovered thread at a glance, beside its row.
    pub(super) fn render_thread_popover(
        &self,
        thread: ThreadId,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.state();
        let t = &self.sidebar.threads;
        let stored = state.thread(thread)?;
        let channel = state.channel(stored.channel)?;
        let project = state.project(channel.project)?;
        let row = t.list.bounds_for_item(ix)?;
        let summary = t.summaries.by_thread.get(&thread);
        let status = self.thread_card_status(thread);
        let now = now_ms();
        let name_of = |a| {
            state
                .agent(a)
                .map_or_else(|| "An agent".to_string(), |a| a.spec.name.clone())
        };
        let (line, token) = match status {
            CardStatus::NeedsYou => {
                let ask: Option<&StoredApproval> = state.pending_approvals(thread).last().copied();
                let text = ask.map_or_else(
                    || "Needs you".to_string(),
                    |a| {
                        let who = state
                            .run(a.run)
                            .map_or_else(|| "An agent".to_string(), |r| name_of(r.agent));
                        format!("Needs you · {who} {}", wants(&a.kind, name_of))
                    },
                );
                (text, "warning")
            }
            CardStatus::Working => {
                let since = t
                    .facts
                    .get(&thread)
                    .and_then(|f| f.working_since)
                    .unwrap_or(now);
                let who: Vec<String> = state
                    .live_runs(thread)
                    .into_iter()
                    .filter(|r| r.status == RunStatus::Working)
                    .map(|r| name_of(r.agent))
                    .collect();
                (
                    format!("Working · {} · {}", who.join(", "), elapsed(now - since)),
                    "accent-blue",
                )
            }
            CardStatus::Done => ("Done".into(), "success"),
            CardStatus::Stopped => ("Stopped".into(), "text-secondary"),
            CardStatus::Failed => ("Failed".into(), "danger"),
            CardStatus::Queued => ("Queued".into(), "text-tertiary"),
            CardStatus::Idle => ("No runs yet".into(), "text-tertiary"),
        };
        let files = summary.map_or(0, |s| s.files);
        let (added, removed) = summary.map_or((0, 0), |s| (s.added, s.removed));
        let tree = summary.map_or(WorktreeState::None, |s| s.worktree);
        let changes_text = if files == 0 && added == 0 && removed == 0 {
            "No changes".to_string()
        } else {
            let state_word = match tree {
                WorktreeState::Uncommitted { .. } => Some("uncommitted"),
                WorktreeState::Committed => Some("committed"),
                WorktreeState::Clean | WorktreeState::None => None,
            };
            let mut text = format!("{} · +{added} −{removed}", plural(files, "file", "files"));
            if let Some(word) = state_word {
                text.push_str(&format!(" · {word}"));
            }
            text
        };
        let agents: Vec<(String, String)> = summary
            .map(|s| s.participants.clone())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| state.agent(a))
            .map(|a| (a.spec.name.clone(), runtime_chip(&a.spec)))
            .collect();
        let activity = summary.map_or_else(String::new, |s| {
            format!(
                "{} · {} · {}",
                plural(s.messages, "message", "messages"),
                plural(s.runs.len() as u64, "run", "runs"),
                span(s.agent_ms)
            )
        });
        let last = t
            .facts
            .get(&thread)
            .map(|f| f.activity)
            .unwrap_or(stored.created_at)
            .max(summary.map_or(0, |s| s.last_activity_at));
        let reviewable = stored.worktree.is_some() && files > 0;
        let x = self.sidebar_width() + ui_px(8.);
        let kv = |key: &'static str, id: &'static str, label: String, value: AnyElement| {
            div()
                .flex()
                .gap(ui_px(12.))
                .px(ui_px(12.))
                .py(scale::row(4.))
                .text_size(scale::text(12.))
                .line_height(scale::line(18.))
                .child(
                    div()
                        .w(ui_px(72.))
                        .flex_none()
                        .text_color(color("text-tertiary"))
                        .child(key),
                )
                .child(
                    div()
                        .id(id)
                        .flex_1()
                        .min_w_0()
                        .text_color(color("text-primary"))
                        .aria_label(label)
                        .child(value)
                        .test_support(),
                )
        };
        let place = format!("{} · #{}", project.name, channel.name);
        let branch = stored.worktree.as_ref().map(|w| w.branch.clone());
        let pop = div()
            .id("thread-popover")
            .w(ui_px(POPOVER_WIDTH))
            .bg(color("surface-raised"))
            .border_1()
            .border_color(color("border-200"))
            .rounded(ui_px(8.))
            .shadow_lg()
            .occlude()
            .aria_label(stored.title.clone())
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.sidebar.threads.popover_hovered = *hovered;
                if *hovered {
                    this.sidebar.threads.linger = None;
                } else {
                    this.close_popover_later(cx);
                }
            }))
            .child(
                div()
                    .px(ui_px(12.))
                    .pt(scale::row(12.))
                    .pb(scale::row(8.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .child(
                        div()
                            .font_weight(weight(FontWeight::SEMIBOLD))
                            .line_height(scale::line(20.))
                            .child(stored.title.clone()),
                    )
                    .child(
                        div()
                            .id("thread-popover-status")
                            .mt(scale::row(2.))
                            .text_size(scale::text(12.))
                            .text_color(color(token))
                            .aria_label(line.clone())
                            .child(line)
                            .test_support(),
                    ),
            )
            .child(
                div()
                    .py(scale::row(8.))
                    .child(kv(
                        "Project",
                        "thread-popover-project",
                        place.clone(),
                        place.into_any_element(),
                    ))
                    .children(branch.map(|b| {
                        kv(
                            "Branch",
                            "thread-popover-branch",
                            b.clone(),
                            div()
                                .font_family(code_family())
                                .text_size(scale::mono(11.))
                                .truncate()
                                .child(b)
                                .into_any_element(),
                        )
                    }))
                    .child(kv(
                        "Changes",
                        "thread-popover-changes",
                        changes_text.clone(),
                        changes_value(files, added, removed, tree).into_any_element(),
                    ))
                    .when(!agents.is_empty(), |list| {
                        let label = agents
                            .iter()
                            .map(|(n, c)| format!("{n} {c}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        list.child(kv(
                            "Agents",
                            "thread-popover-agents",
                            label,
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(ui_px(6.))
                                .children(agents.into_iter().map(|(name, chip)| {
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(ui_px(4.))
                                        .child(name)
                                        .child(super::super::common::chip(chip))
                                }))
                                .into_any_element(),
                        ))
                    })
                    .when(!activity.is_empty(), |list| {
                        list.child(kv(
                            "Activity",
                            "thread-popover-activity",
                            activity.clone(),
                            activity.clone().into_any_element(),
                        ))
                    })
                    .child({
                        let last = ago(last, now);
                        kv(
                            "Last",
                            "thread-popover-last",
                            last.clone(),
                            last.into_any_element(),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(ui_px(6.))
                    .px(ui_px(12.))
                    .py(scale::row(8.))
                    .border_t_1()
                    .border_color(color("border-100"))
                    .child(
                        small_button("thread-popover-open", "Open", false)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.sidebar.threads.hovered = None;
                                this.open_place(Place::Thread(thread), window, cx);
                            }))
                            .test_support(),
                    )
                    .when(reviewable, |bar| {
                        bar.child(
                            small_button("thread-popover-review", "Review changes", true)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.sidebar.threads.hovered = None;
                                    this.leave_pages(cx);
                                    this.open_review(thread, window, cx);
                                }))
                                .test_support(),
                        )
                    }),
            )
            .test_support();
        Some(
            deferred(
                anchored()
                    .position(point(x, row.origin.y - scale::row(POPOVER_ABOVE)))
                    .snap_to_window_with_margin(ui_px(8.))
                    .child(pop),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }
}

/// "4 files · +88 −51 · uncommitted", the counts in their colours.
fn changes_value(files: u32, added: u32, removed: u32, tree: WorktreeState) -> Div {
    if files == 0 && added == 0 && removed == 0 {
        return div().child("No changes");
    }
    let word = match tree {
        WorktreeState::Uncommitted { .. } => Some("uncommitted"),
        WorktreeState::Committed => Some("committed"),
        WorktreeState::Clean | WorktreeState::None => None,
    };
    div()
        .flex()
        .flex_wrap()
        .gap(ui_px(4.))
        .child(format!("{} ·", plural(files, "file", "files")))
        .child(
            div()
                .font_family(code_family())
                .text_color(color("success"))
                .child(format!("+{added}")),
        )
        .child(
            div()
                .font_family(code_family())
                .text_color(color("danger"))
                .child(format!("−{removed}")),
        )
        .children(word.map(|w| format!("· {w}")))
}

/// The popover's 24 px button, outlined or ghost.
fn small_button(id: &'static str, label: &'static str, ghost: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(ui_px(24.))
        .px(ui_px(8.))
        .rounded(ui_px(6.))
        .border_1()
        .cursor_pointer()
        .whitespace_nowrap()
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .when(ghost, |b| {
            b.border_color(transparent_black())
                .text_color(color("text-secondary"))
        })
        .when(!ghost, |b| {
            b.border_color(color("border-200"))
                .bg(color("bg-100"))
                .text_color(color("text-primary"))
        })
        .hover(|b| b.bg(color("surface-300")))
        .aria_label(label)
        .child(label)
}
