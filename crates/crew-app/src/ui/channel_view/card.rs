//! One thread's card, its actions and its menus.

use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ThreadId};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::common::{
    clean_line, dot, elapsed, generating_title, monogram, now_ms, pulsing, runtime_badge_for,
    runtime_chip, spinner, thread_title_text,
};
use super::super::run_card::summarize;
use super::super::workspace::Workspace;
use super::{
    Card, CardStatus, card_time, counts_line, pill_tokens, stop_label, strip_layout, working_line,
    worktree_word,
};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// The selected card's outline, between border-200 and the text.
const SELECTED_BORDER: u32 = 0x3a3a3a;
/// The run strip's width.
const STRIP_WIDTH: f32 = 112.;
/// The space between a run strip's segments.
const STRIP_GAP: f32 = 2.;
/// Most runtime chips a card shows.
const CHIPS_MAX: usize = 2;

fn pill(thread: ThreadId, status: CardStatus) -> impl IntoElement {
    let (text, fill, border) = pill_tokens(status);
    let icon = |name: IconName| {
        Icon::new(name)
            .size(ui_px(11.))
            .text_color(color(text))
            .into_any_element()
    };
    let mark = match status {
        CardStatus::Working => Some(
            spinner(SharedString::from(format!("pill-spin-{thread}")), 11., text)
                .into_any_element(),
        ),
        CardStatus::NeedsYou => Some(icon(IconName::TriangleAlert)),
        CardStatus::Done => Some(icon(IconName::Check)),
        CardStatus::Stopped => Some(icon(IconName::Square)),
        CardStatus::Failed => Some(icon(IconName::CircleX)),
        CardStatus::Queued | CardStatus::Idle => None,
    };
    div()
        .id(SharedString::from(format!("thread-status-{thread}")))
        .flex()
        .flex_none()
        .items_center()
        .gap(ui_px(5.))
        .h(scale::row(20.))
        .px(ui_px(7.))
        .rounded_full()
        .border_1()
        .border_color(color(border))
        .when_some(fill, |pill, fill| pill.bg(color(fill)))
        .text_size(scale::text(11.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color(text))
        .aria_label(status.word())
        .children(mark)
        .child(status.word())
        .test_support()
}

/// What a thread's menus need to know.
#[derive(Clone)]
struct MenuFor {
    thread: ThreadId,
    archived: bool,
    branch: Option<String>,
    has_worktree: bool,
    /// The thread's worktree folder, when it has one.
    worktree: Option<std::path::PathBuf>,
    /// "Stop {Agent}" or "Stop all in thread", while agents work.
    stop: Option<String>,
}

/// A menu row with its shortcut on the right.
fn menu_row(label: &'static str, keys: Option<&'static str>, danger: bool) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        div()
            .flex()
            .items_center()
            .w_full()
            .gap(ui_px(8.))
            .when(danger, |row| row.text_color(color("danger")))
            .child(div().flex_1().child(label))
            .children(keys.map(|keys| {
                div()
                    .font_family(code_family())
                    .text_size(scale::mono(11.))
                    .text_color(color("text-tertiary"))
                    .child(keys)
            }))
    })
}

/// The overflow menu's items, after "Open" and "Stop {Agent}" in the
/// right-click menu.
fn thread_menu(
    menu: PopupMenu,
    this: WeakEntity<Workspace>,
    item: &MenuFor,
    right_click: bool,
) -> PopupMenu {
    let thread = item.thread;
    let act = |f: fn(&mut Workspace, ThreadId, &mut Window, &mut Context<Workspace>)| {
        let this = this.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            this.update(cx, |this, cx| f(this, thread, window, cx)).ok();
        }
    };
    let mut menu = menu;
    if right_click {
        menu = menu.item(
            menu_row("Open", None, false)
                .on_click(act(|this, t, window, cx| this.open_thread(t, window, cx))),
        );
        if let Some(label) = item.stop.clone() {
            menu = menu.item(
                PopupMenuItem::new(label)
                    .on_click(act(|this, t, window, cx| this.stop_thread(t, window, cx))),
            );
        }
        menu = menu.separator();
    }
    menu = menu.item(
        menu_row("Rename", Some("⌘R"), false).on_click(act(|this, t, window, cx| {
            this.open_rename_dialog(t, window, cx)
        })),
    );
    if let Some(branch) = item.branch.clone() {
        menu = menu.item(
            PopupMenuItem::new("Copy branch name").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()));
            }),
        );
    }
    if item.worktree.is_some() {
        menu = menu.item(
            PopupMenuItem::new("Open worktree in terminal").on_click(act(|this, t, window, cx| {
                this.open_worktree_in_terminal(t, window, cx)
            })),
        );
    }
    menu =
        if item.archived {
            menu.item(
                PopupMenuItem::new("Unarchive").on_click(act(|this, t, window, cx| {
                    this.unarchive_thread(t, window, cx)
                })),
            )
        } else {
            menu.item(menu_row("Archive", Some("⌘⌫"), false).on_click(act(
                |this, t, window, cx| this.open_archive_dialog(t, window, cx),
            )))
        };
    if item.has_worktree {
        menu = menu
            .separator()
            .item(menu_row("Discard worktree…", None, true).on_click(act(
                |this, t, window, cx| this.open_discard_dialog(t, window, cx),
            )));
    }
    menu
}

impl Workspace {
    /// Opens Terminal in the thread's worktree.
    fn open_worktree_in_terminal(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self
            .state()
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|w| w.path.clone())
        else {
            return;
        };
        let args = vec!["-a".into(), "Terminal".into(), path.into_os_string()];
        let failed = "Terminal didn't open. Check that the worktree still exists.".to_string();
        self.run_open(args, failed, window, cx);
    }

    /// Stops the agents working in `thread`.
    pub(super) fn stop_thread(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let runs: Vec<_> = self
            .state()
            .live_runs(thread)
            .into_iter()
            .filter(|r| r.status == RunStatus::Working)
            .map(|r| r.id)
            .collect();
        for run in runs {
            self.stop_run(run, window, cx);
        }
    }

    /// Brings an archived thread back.
    pub(crate) fn unarchive_thread(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request(
            crew_proto::Request::ArchiveThread {
                thread,
                archived: false,
            },
            window,
            cx,
        );
    }

    pub(super) fn render_card(&self, thread: ThreadId, cx: &mut Context<Self>) -> AnyElement {
        let Some(card) = self.channel_view.cards.get(&thread) else {
            return div().into_any_element();
        };
        let state = self.state();
        let now = now_ms();
        let id = card.thread;
        // A card keeps its actions while its overflow menu is open, which
        // the pointer leaves the card for.
        let hovered =
            self.channel_view.hovered == Some(id) || self.channel_view.menu_open == Some(id);
        let selected = self.channel_view.selected == Some(id);
        let working_runs: Vec<_> = state
            .live_runs(id)
            .into_iter()
            .filter(|r| r.status == RunStatus::Working)
            .collect();
        let working = working_runs.first().copied();
        let mut working_names: Vec<String> = Vec::new();
        for run in &working_runs {
            if let Some(agent) = state.agent(run.agent)
                && !working_names.contains(&agent.spec.name)
            {
                working_names.push(agent.spec.name.clone());
            }
        }
        let working_agent = working_names.first().cloned();
        let summary = &card.summary;

        let excerpt = match (card.status, &working_agent) {
            (CardStatus::Working, Some(agent)) => {
                let step = working
                    .filter(|_| state.is_watched(id))
                    .and_then(|run| summarize(state.events(id), run.id).steps.pop());
                working_line(agent, step.as_ref())
            }
            (CardStatus::Queued, _) => "Waiting to start.".to_string(),
            _ => clean_line(&summary.excerpt),
        };
        let running = working.map(|r| elapsed(now - r.started_at));
        let counts = counts_line(summary, running.as_deref());
        // Review changes shows only what isn't committed yet.
        let has_changes = matches!(
            summary.worktree,
            crew_proto::WorktreeState::Uncommitted { .. }
        );

        let top_right = if hovered {
            self.render_card_actions(card, working_agent.is_some(), has_changes, cx)
                .into_any_element()
        } else {
            let time = card_time(card.last_activity, now);
            div()
                .id(SharedString::from(format!("thread-time-{id}")))
                .flex_none()
                .text_size(scale::text(12.))
                .text_color(color("text-tertiary"))
                .aria_label(time.clone())
                .child(time)
                .test_support()
                .into_any_element()
        };

        let menu = MenuFor {
            thread: id,
            archived: card.archived,
            branch: card.branch.clone(),
            has_worktree: card.branch.is_some(),
            worktree: self
                .state()
                .thread(id)
                .and_then(|t| t.worktree.as_ref())
                .map(|w| w.path.clone()),
            stop: stop_label(&working_names),
        };
        let this = cx.entity().downgrade();
        div()
            .id(SharedString::from(format!("thread-{id}")))
            .flex()
            .flex_col()
            .gap(ui_px(10.))
            .px(ui_px(14.))
            .py(scale::row(12.))
            .border_1()
            .rounded(ui_px(8.))
            .map(|card| {
                if selected {
                    card.bg(color("surface-200"))
                        .border_color(rgb(SELECTED_BORDER))
                } else if hovered {
                    card.bg(color("surface-100"))
                        .border_color(color("border-200"))
                } else {
                    card.bg(color("bg-100")).border_color(color("border-100"))
                }
            })
            .cursor_pointer()
            .aria_label(card.title.clone())
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                let view = &mut this.channel_view;
                if *hovered {
                    view.hovered = Some(id);
                } else if view.hovered == Some(id) {
                    view.hovered = None;
                }
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(id, window, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .h(scale::row(24.))
                    .child(pill(id, card.status))
                    .child(
                        div()
                            .id(SharedString::from(format!("thread-title-{id}")))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(scale::text(14.))
                            .line_height(scale::line(20.))
                            .font_weight(weight(FontWeight::SEMIBOLD))
                            .aria_label(card.title.clone())
                            .when(card.generating, |el| {
                                generating_title(el).aria_description("Generating a title")
                            })
                            .child(thread_title_text(
                                format!("thread-title-{id}"),
                                &card.title,
                                card.generating,
                                self.state().title_just_landed(id),
                            ))
                            .test_support(),
                    )
                    .child(top_right),
            )
            .child(
                div()
                    .id(SharedString::from(format!("thread-excerpt-{id}")))
                    .h(scale::row(40.))
                    .line_height(scale::line(20.))
                    .overflow_hidden()
                    .text_color(color("text-secondary"))
                    .aria_label(excerpt.clone())
                    .child(excerpt)
                    .test_support(),
            )
            .child(self.render_card_people(card, working.map(|r| r.agent)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(14.))
                    .pt(scale::row(10.))
                    .border_t_1()
                    .border_color(color("border-100"))
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .whitespace_nowrap()
                    .child(
                        div()
                            .id(SharedString::from(format!("thread-counts-{id}")))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .aria_label(counts.clone())
                            .child(render_counts(card, running))
                            .test_support(),
                    )
                    .child(self.render_strip(card)),
            )
            .test_support()
            .context_menu(move |popup, _, _| thread_menu(popup, this.clone(), &menu, true))
            .into_any_element()
    }

    /// Open, Review changes (with changes), Stop (while working) and the
    /// overflow menu, shown in place of the time while hovered.
    fn render_card_actions(
        &self,
        card: &Card,
        working: bool,
        has_changes: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = card.thread;
        let menu = MenuFor {
            thread: id,
            archived: card.archived,
            branch: card.branch.clone(),
            has_worktree: card.branch.is_some(),
            worktree: self
                .state()
                .thread(id)
                .and_then(|t| t.worktree.as_ref())
                .map(|w| w.path.clone()),
            stop: None,
        };
        let this = cx.entity().downgrade();
        let opened = this.clone();
        div()
            .id(SharedString::from(format!("thread-actions-{id}")))
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(4.))
            // A click on an action, the menu button included, isn't a
            // click on the card.
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                Button::new(SharedString::from(format!("thread-open-{id}")))
                    .outline()
                    .xsmall()
                    .label("Open")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_thread(id, window, cx);
                    })),
            )
            .when(has_changes, |row| {
                row.child(
                    Button::new(SharedString::from(format!("thread-review-{id}")))
                        .ghost()
                        .xsmall()
                        .label("Review changes")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_review(id, window, cx);
                        })),
                )
            })
            .when(working, |row| {
                row.child(
                    Button::new(SharedString::from(format!("thread-stop-{id}")))
                        .outline()
                        .xsmall()
                        .text_color(color("danger"))
                        .icon(IconName::Square)
                        .label("Stop")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.stop_thread(id, window, cx);
                        })),
                )
            })
            .child(
                Button::new(SharedString::from(format!("thread-more-{id}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Ellipsis)
                    .accessibility_label("More actions")
                    .dropdown_menu(move |popup, _, _| {
                        thread_menu(popup, this.clone(), &menu, false)
                    })
                    .on_open_change(move |open, _, cx| {
                        opened
                            .update(cx, |this, cx| {
                                let view = &mut this.channel_view;
                                if *open {
                                    view.menu_open = Some(id);
                                } else if view.menu_open == Some(id) {
                                    view.menu_open = None;
                                }
                                cx.notify();
                            })
                            .ok();
                    }),
            )
    }

    /// You and the agents that ran here, then their runtimes, the branch
    /// and where its worktree stands.
    fn render_card_people(&self, card: &Card, working: Option<AgentId>) -> impl IntoElement {
        let state = self.state();
        let id = card.thread;
        let agents: Vec<_> = card
            .summary
            .participants
            .iter()
            .filter_map(|a| state.agent(*a))
            .collect();
        let mut names = vec!["You".to_string()];
        names.extend(agents.iter().map(|a| a.spec.name.clone()));
        let mut chips: Vec<(crew_core::agent::Runtime, String)> = Vec::new();
        for agent in &agents {
            let chip = runtime_chip(&agent.spec);
            if !chips.iter().any(|(_, c)| *c == chip) {
                chips.push((agent.spec.runtime.clone(), chip));
            }
        }
        chips.truncate(CHIPS_MAX);
        let avatar = |el: Div| el.size(ui_px(22.)).border_color(color("bg-100"));
        let stack = std::iter::once(
            avatar(monogram("You", 22.))
                .rounded_full()
                .bg(color("border-200"))
                .into_any_element(),
        )
        .chain(agents.iter().map(|a| {
            let live = working == Some(a.id);
            avatar(monogram(&a.spec.name, 22.))
                .ml(ui_px(-6.))
                .when(live, |el| el.border_color(color("accent-blue")))
                .into_any_element()
        }));
        let (word, token) = worktree_word(card.summary.worktree, card.branch.is_some());
        let uncommitted = matches!(
            card.summary.worktree,
            crew_proto::WorktreeState::Uncommitted { .. }
        );
        div()
            .flex()
            .items_center()
            .gap(ui_px(10.))
            .child(
                div()
                    .id(SharedString::from(format!("thread-people-{id}")))
                    .flex()
                    .flex_none()
                    .aria_label(names.join(", "))
                    .children(stack)
                    .test_support(),
            )
            .children(
                chips
                    .into_iter()
                    .map(|(runtime, chip)| runtime_badge_for(&runtime, chip)),
            )
            .child(div().flex_1())
            .children(card.branch.clone().map(|branch| {
                div()
                    .id(SharedString::from(format!("thread-branch-{id}")))
                    .flex_shrink(1.)
                    .min_w_0()
                    .truncate()
                    .px(ui_px(6.))
                    .rounded(ui_px(4.))
                    .bg(color("surface-300"))
                    .font_family(code_family())
                    .text_size(scale::mono(11.))
                    .line_height(scale::mono_line(16.))
                    .text_color(color("text-secondary"))
                    .aria_label(branch.clone())
                    .child(branch)
                    .test_support()
            }))
            .child(
                div()
                    .id(SharedString::from(format!("thread-worktree-{id}")))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(ui_px(5.))
                    .text_size(scale::text(11.))
                    .text_color(color(token))
                    .aria_label(word.clone())
                    .when(uncommitted, |row| row.child(dot("text-secondary")))
                    .child(word)
                    .test_support(),
            )
    }

    /// One segment per run, as wide as its share of the busiest thread's
    /// agent time, coloured by how it ended.
    fn render_strip(&self, card: &Card) -> impl IntoElement {
        let state = self.state();
        let id = card.thread;
        let waiting = !state.pending_approvals(id).is_empty();
        let now = now_ms();
        let live_ms = state
            .live_runs(id)
            .into_iter()
            .find(|r| r.status == RunStatus::Working)
            .map(|r| now - r.started_at);
        let ms: Vec<i64> = card
            .summary
            .runs
            .iter()
            .map(|r| match (&r.status, live_ms) {
                (RunStatus::Working, Some(live)) => r.ms.max(live),
                _ => r.ms,
            })
            .collect();
        let total: i64 = ms.iter().sum();
        let scale = self.channel_view.scale.max(total);
        let layout = strip_layout(&ms, scale, STRIP_WIDTH, STRIP_GAP);
        let segments = layout
            .into_iter()
            .map(|(ix, width)| (ix, &card.summary.runs[ix], width))
            .map(|(ix, run, width)| {
                let token = match run.status {
                    RunStatus::Done => "success",
                    RunStatus::Stopped | RunStatus::Interrupted => "text-tertiary",
                    RunStatus::Failed => "danger",
                    RunStatus::Working if waiting => "warning",
                    RunStatus::Working => "accent-blue",
                    RunStatus::Queued => "border-200",
                };
                let bar = div()
                    .flex_none()
                    .w(ui_px(width))
                    .h(ui_px(4.))
                    .rounded(ui_px(1.))
                    .bg(color(token));
                if run.status == RunStatus::Working && !waiting {
                    pulsing(SharedString::from(format!("strip-{id}-{ix}")), bar).into_any_element()
                } else {
                    bar.into_any_element()
                }
            });
        let n = card.summary.runs.len();
        div()
            .id(SharedString::from(format!("thread-strip-{id}")))
            .flex()
            .flex_none()
            .gap(ui_px(STRIP_GAP))
            .w(ui_px(STRIP_WIDTH))
            .h(ui_px(4.))
            .overflow_hidden()
            .rounded(ui_px(1.))
            .bg(color("border-100"))
            .aria_label(if n == 1 {
                "1 run".to_string()
            } else {
                format!("{n} runs")
            })
            .children(segments)
            .test_support()
    }
}

/// The counts row, with numbers bright, lines in green and red, and the
/// running time in blue.
fn render_counts(card: &Card, running: Option<String>) -> impl IntoElement {
    let s = &card.summary;
    let runs = u32::try_from(s.runs.len()).unwrap_or(u32::MAX);
    let part = |n: u32, one: &'static str, many: &'static str| {
        div()
            .flex()
            .flex_none()
            .gap(ui_px(4.))
            .child(
                div()
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color("text-primary"))
                    .child(super::thousands(n)),
            )
            .child(if n == 1 { one } else { many })
    };
    let files = part(s.files, "file", "files").when(s.files > 0, |files| {
        files
            .child(
                div()
                    .font_family(code_family())
                    .text_color(color("success"))
                    .child(format!("+{}", super::thousands(s.added))),
            )
            .child(
                div()
                    .font_family(code_family())
                    .text_color(color("danger"))
                    .child(format!("−{}", super::thousands(s.removed))),
            )
    });
    let last = match running {
        Some(elapsed) => div()
            .flex()
            .flex_none()
            .gap(ui_px(4.))
            .text_color(color("accent-blue"))
            .child("running")
            .child(div().font_family(code_family()).child(elapsed)),
        None => div()
            .flex()
            .flex_none()
            .gap(ui_px(4.))
            .child(
                div()
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color("text-primary"))
                    .child(super::super::running_view::usage_time(s.agent_ms)),
            )
            .child("agent time"),
    };
    div()
        .flex()
        .items_center()
        .gap(ui_px(14.))
        .overflow_hidden()
        .child(part(s.messages, "message", "messages"))
        .child(part(runs, "run", "runs"))
        .child(part(s.handoffs, "handoff", "handoffs"))
        .child(files)
        .child(last)
}
