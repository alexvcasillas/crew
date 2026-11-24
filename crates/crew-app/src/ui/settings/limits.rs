//! Settings › Handoffs and limits: how many agents run at once, whether
//! new runs start, and the handoff policy used when nothing narrower sets
//! one. Each change is sent at once.

use crew_core::policy::HandoffPolicy;
use crew_proto::{Limits, Request};
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Page, field, form_error, page_scroll, page_title, segment, segmented};
use crate::ui::workspace::Workspace;

/// What "Agents at once" offers.
pub const AGENTS_AT_ONCE: [u32; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
/// What "Handoffs per thread" offers.
pub const BUDGETS: [u32; 5] = [1, 2, 3, 5, 10];

const AT_ONCE_HELP: &str = "More runs at once use more of your plan. Extra runs wait in the queue.";
const NEW_RUNS_HELP: &str = "Paused keeps live runs going and holds new ones in the queue.";
const POLICY_HELP: &str = "Used when an agent, channel or thread doesn't set its own.";
const BUDGET_HELP: &str = "After this many, Crew pauses and asks you.";

impl Workspace {
    /// Sends the whole set of limits. They show at once and stay until
    /// crewd reports them; a failure goes back to crewd's and says why,
    /// on the page if it is open, else as a notification.
    pub(crate) fn set_limits(
        &mut self,
        limits: Limits,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *self.state().limits() == limits {
            return;
        }
        self.limits_error = None;
        self.state_mut().expect_limits(limits);
        let answer = self.client.request(Request::SetLimits { limits });
        self.spawn_request(window, cx, async move |this, cx| {
            let Err(error) = answer.await else {
                return Ok(());
            };
            let inline = this
                .update(cx, |this, cx| {
                    this.state_mut().drop_limits();
                    cx.notify();
                    let shown = this
                        .settings
                        .as_ref()
                        .is_some_and(|s| s.page == Page::Limits);
                    if shown {
                        this.limits_error = Some(error.clone());
                    }
                    shown
                })
                .unwrap_or(true);
            if inline { Ok(()) } else { Err((error, None)) }
        });
        cx.notify();
    }

    pub(super) fn render_limits_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let limits = *self.state().limits();
        let budget_on = limits.handoff_policy == HandoffPolicy::Budget;

        let this = cx.entity().downgrade();
        let at_once = menu_button("limits-max-runs", limits.max_runs, true).dropdown_menu(
            move |menu, _, _| {
                AGENTS_AT_ONCE.iter().fold(menu, |menu, n| {
                    let (this, n) = (this.clone(), *n);
                    menu.item(
                        PopupMenuItem::new(n.to_string()).on_click(move |_, window, cx| {
                            // Built on what is known when picked, not
                            // when the menu was drawn, so a change from
                            // another app since then stays.
                            this.update(cx, |this, cx| {
                                let limits = *this.state().limits();
                                this.set_limits(
                                    Limits {
                                        max_runs: n,
                                        ..limits
                                    },
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                        }),
                    )
                })
            },
        );

        // A budget set another way, such as from the CLI, is offered too.
        let mut budgets = BUDGETS.to_vec();
        if !budgets.contains(&limits.handoff_budget) {
            budgets.push(limits.handoff_budget);
            budgets.sort_unstable();
        }
        let this = cx.entity().downgrade();
        let budget = menu_button("limits-budget", limits.handoff_budget, budget_on).dropdown_menu(
            move |menu, _, _| {
                budgets.iter().fold(menu, |menu, n| {
                    let (this, n) = (this.clone(), *n);
                    menu.item(
                        PopupMenuItem::new(n.to_string()).on_click(move |_, window, cx| {
                            // Built on what is known when picked, not
                            // when the menu was drawn, so a change from
                            // another app since then stays.
                            this.update(cx, |this, cx| {
                                let limits = *this.state().limits();
                                this.set_limits(
                                    Limits {
                                        handoff_budget: n,
                                        ..limits
                                    },
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                        }),
                    )
                })
            },
        );

        let runs: Vec<AnyElement> = [
            (false, "limits-runs-running", "Running"),
            (true, "limits-runs-paused", "Paused"),
        ]
        .into_iter()
        .map(|(paused, id, label)| {
            segment(id, label, limits.paused == paused)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let limits = *this.state().limits();
                    this.set_limits(Limits { paused, ..limits }, window, cx);
                }))
                .test_support()
                .into_any_element()
        })
        .collect();
        let policies: Vec<AnyElement> = [
            (HandoffPolicy::AskFirst, "limits-policy-ask", "Ask first"),
            (
                HandoffPolicy::Budget,
                "limits-policy-budget",
                "Run with budget",
            ),
        ]
        .into_iter()
        .map(|(policy, id, label)| {
            segment(id, label, limits.handoff_policy == policy)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let limits = *this.state().limits();
                    this.set_limits(
                        Limits {
                            handoff_policy: policy,
                            ..limits
                        },
                        window,
                        cx,
                    );
                }))
                .test_support()
                .into_any_element()
        })
        .collect();

        page_scroll("settings-limits")
            .child(page_title("Handoffs and limits"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(px(760.))
                    .child(row(
                        "limits-row-max-runs",
                        "Agents at once",
                        AT_ONCE_HELP,
                        value("limits-max-runs-value", limits.max_runs, at_once),
                    ))
                    .child(row(
                        "limits-row-new-runs",
                        "New runs",
                        NEW_RUNS_HELP,
                        div().flex().child(segmented(runs).w(px(240.))),
                    ))
                    .child(row(
                        "limits-row-policy",
                        "Bringing in other agents",
                        POLICY_HELP,
                        div().flex().child(segmented(policies).w(px(320.))),
                    ))
                    .child(row(
                        "limits-row-budget",
                        "Handoffs per thread",
                        BUDGET_HELP,
                        value("limits-budget-value", limits.handoff_budget, budget),
                    ))
                    .when_some(self.limits_error.clone(), |col, error| {
                        col.child(div().pt(px(12.)).child(form_error("limits-error", error)))
                    }),
            )
            .into_any_element()
    }
}

/// A form row, labelled with its label so tests can find it.
fn row(
    id: &'static str,
    label: &'static str,
    help: &'static str,
    content: impl IntoElement,
) -> impl IntoElement {
    div()
        .id(id)
        .aria_label(label)
        .child(field(label, Some(help), content))
        .test_support()
}

/// A small outline button showing `n`, for a menu of numbers.
fn menu_button(id: &'static str, n: u32, enabled: bool) -> Button {
    Button::new(id)
        .outline()
        .small()
        .label(n.to_string())
        .icon(IconName::ChevronDown)
        .disabled(!enabled)
}

/// A menu button labelled with its value, so tests can read it.
fn value(id: &'static str, n: u32, button: impl IntoElement) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .aria_label(n.to_string())
        .child(button)
        .test_support()
}
