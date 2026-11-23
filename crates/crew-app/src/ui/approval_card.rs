//! Cards for what waits on you in a thread: a handoff to approve, a tool
//! to allow, or a used-up handoff budget. Once decided, a card folds into
//! one line.

use crew_core::agent::PermissionMode;
use crew_core::ids::{AgentId, ApprovalId, ThreadId};
use crew_core::store::{ApprovalKind, ApprovalStatus, StoredApproval};
use crew_proto::Request;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{chip, clock, monogram, runtime_chip};
use super::thread_settings::effective;
use super::thread_view::indented;
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::theme::color;

/// Input lines a permission card shows until you ask for all of it.
const INPUT_LINES: usize = 6;
/// How many more handoffs "Allow 3 more" lets run.
pub const MORE_HANDOFFS: u32 = 3;

/// A handoff brief being edited before approving.
pub(super) struct BriefEditor {
    pub(super) approval: ApprovalId,
    pub(super) input: Entity<TextareaState>,
}

/// The text a tool's input shows as: its command if it has one, else its
/// JSON, pretty.
pub fn input_text(input: &serde_json::Value) -> String {
    match input.get("command").and_then(|c| c.as_str()) {
        Some(command) => command.to_string(),
        None => serde_json::to_string_pretty(input).unwrap_or_default(),
    }
}

/// The one-line summary of a decided approval, such as `Handoff to Scout
/// approved by you · 10:05`.
pub fn decided_text(approval: &StoredApproval, name_of: impl Fn(AgentId) -> String) -> String {
    let time = approval.decided_at.map(clock).unwrap_or_default();
    let what = match (&approval.kind, approval.status) {
        (ApprovalKind::Handoff { to, .. }, ApprovalStatus::Approved) => {
            format!("Handoff to {} approved by you", name_of(*to))
        }
        (ApprovalKind::Handoff { to, .. }, ApprovalStatus::Declined) => {
            format!("Handoff to {} declined by you", name_of(*to))
        }
        (ApprovalKind::Handoff { to, .. }, _) => {
            format!("Handoff to {} no longer needed", name_of(*to))
        }
        (ApprovalKind::Permission { tool, .. }, ApprovalStatus::Approved) => {
            format!("{tool} allowed by you")
        }
        (ApprovalKind::Permission { tool, .. }, ApprovalStatus::Declined) => {
            format!("{tool} denied by you")
        }
        (ApprovalKind::Permission { tool, .. }, _) => format!("{tool} no longer needed"),
    };
    if time.is_empty() {
        what
    } else {
        format!("{what} · {time}")
    }
}

impl Workspace {
    fn name_of(&self, agent: AgentId) -> String {
        self.state()
            .agent(agent)
            .map_or_else(|| "An agent".to_string(), |a| a.spec.name.clone())
    }

    /// Approves or declines, with `brief` replacing a handoff's brief.
    /// Each approval is sent once; if crewd refuses, you can try again.
    pub(super) fn decide(
        &mut self,
        approval: ApprovalId,
        approve: bool,
        brief: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.deciding.insert(approval) {
            return;
        }
        if self
            .editing
            .as_ref()
            .is_some_and(|e| e.approval == approval)
        {
            self.editing = None;
        }
        let answer = self.client.request(Request::Decide {
            approval,
            approve,
            brief,
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await.map(drop);
            if result.is_err() {
                this.update(cx, |this, _| this.deciding.remove(&approval))
                    .ok();
            }
            result.map_err(|e| (e, None))
        });
        cx.notify();
    }

    /// Approves the brief you are editing, else the newest pending
    /// handoff or permission ask in the open thread (⌘↵). ⌘↵ is bound for
    /// the whole window, so text fields don't take it.
    pub(super) fn approve_latest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.nav.thread else {
            return;
        };
        let state = self.state();
        let pending = state.pending_approvals(thread);
        let edited = self
            .editing
            .as_ref()
            .map(|e| e.approval)
            .filter(|id| pending.iter().any(|a| a.id == *id));
        let latest = edited.or_else(|| {
            pending
                .iter()
                .rev()
                .filter(|a| !self.deciding.contains(&a.id))
                .find(|a| !matches!(a.kind, ApprovalKind::Handoff { paused: true, .. }))
                .map(|a| a.id)
        });
        if let Some(approval) = latest {
            let brief = self.edited_brief(approval, cx);
            self.decide(approval, true, brief, window, cx);
        }
    }

    fn edited_brief(&self, approval: ApprovalId, cx: &App) -> Option<String> {
        self.editing
            .as_ref()
            .filter(|e| e.approval == approval)
            .map(|e| e.input.read(cx).value().to_string())
    }

    fn edit_brief(
        &mut self,
        approval: ApprovalId,
        brief: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .default_value(brief)
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.editing = Some(BriefEditor { approval, input });
        cx.notify();
    }

    /// An approval's card while pending, or its one line once decided.
    pub(super) fn render_approval(
        &self,
        approval: &StoredApproval,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if approval.status != ApprovalStatus::Pending {
            return self.render_decided(approval).into_any_element();
        }
        match &approval.kind {
            ApprovalKind::Handoff { paused: true, .. } => {
                self.render_paused(approval, cx).into_any_element()
            }
            ApprovalKind::Handoff {
                from, to, brief, ..
            } => self
                .render_handoff(approval, *from, *to, brief, cx)
                .into_any_element(),
            ApprovalKind::Permission { tool, input, .. } => self
                .render_permission(approval, tool, input, cx)
                .into_any_element(),
        }
    }

    fn render_decided(&self, approval: &StoredApproval) -> impl IntoElement + use<> {
        let text = decided_text(approval, |a| self.name_of(a));
        let (icon, token) = match approval.status {
            ApprovalStatus::Approved => (IconName::CircleCheck, "success"),
            ApprovalStatus::Declined => (IconName::CircleX, "text-tertiary"),
            _ => (IconName::Minus, "text-tertiary"),
        };
        indented(
            div()
                .id(format!("approval-{}", approval.id))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(color("text-tertiary"))
                .aria_label(text.clone())
                .child(Icon::new(icon).size(px(12.)).text_color(color(token)))
                .child(text)
                .test_support(),
        )
    }

    fn render_handoff(
        &self,
        approval: &StoredApproval,
        from: AgentId,
        to: AgentId,
        brief: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let state = self.state();
        let id = approval.id;
        let target = state.agent(to);
        let to_name = self.name_of(to);
        let wants = format!("{} wants to bring in {to_name}", self.name_of(from));
        let (used, budget) = state
            .thread(approval.thread)
            .map(|t| {
                let channel = state
                    .channel(t.channel)
                    .map_or((None, None), |c| (c.handoff_policy, c.handoff_budget));
                (t.handoffs_used, effective(t, channel).1)
            })
            .unwrap_or((0, 0));
        let read_only =
            target.is_some_and(|a| matches!(a.spec.permission_mode, PermissionMode::ReadOnly));
        let mode = if read_only {
            "runs read-only"
        } else {
            "can edit files"
        };
        let editing = self.editing.as_ref().filter(|e| e.approval == id);
        let brief_box = match editing {
            Some(editor) => div()
                .id(format!("approval-brief-editor-{id}"))
                .border_1()
                .border_color(color("border-200"))
                .rounded(px(6.))
                .bg(color("bg-100"))
                .p(px(4.))
                .font_family(mono_font())
                .child(Textarea::new(&editor.input).appearance(false))
                .test_support()
                .into_any_element(),
            None => mono_box(format!("approval-brief-{id}"), brief.to_string()).into_any_element(),
        };
        let buttons: Vec<AnyElement> = if editing.is_some() {
            vec![
                Button::new(format!("approval-save-{id}"))
                    .primary()
                    .small()
                    .label("Save & approve")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let brief = this.edited_brief(id, cx);
                        this.decide(id, true, brief, window, cx);
                    }))
                    .into_any_element(),
                decline(id, "Decline", cx).into_any_element(),
            ]
        } else {
            let brief = brief.to_string();
            vec![
                Button::new(format!("approval-approve-{id}"))
                    .primary()
                    .small()
                    .label("Approve")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.decide(id, true, None, window, cx);
                    }))
                    .into_any_element(),
                Button::new(format!("approval-edit-{id}"))
                    .outline()
                    .small()
                    .label("Edit brief")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit_brief(id, brief.clone(), window, cx);
                    }))
                    .into_any_element(),
                decline(id, "Decline", cx).into_any_element(),
            ]
        };
        needs_you_card(id, &wants)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(monogram(&to_name, 24.))
                    .child(div().font_weight(FontWeight::MEDIUM).child(to_name.clone()))
                    .when_some(target.map(|a| a.spec.role.clone()), |row, role| {
                        row.child(
                            div()
                                .text_size(px(12.))
                                .text_color(color("text-secondary"))
                                .child(role),
                        )
                    })
                    .when_some(target.map(|a| runtime_chip(&a.spec)), |row, c| {
                        row.child(chip(c))
                    }),
            )
            .child(brief_box)
            .child({
                let note =
                    format!("This thread: {used} of {budget} handoffs used · {to_name} {mode}");
                div()
                    .id(format!("approval-note-{id}"))
                    .text_size(px(12.))
                    .text_color(color("text-secondary"))
                    .aria_label(note.clone())
                    .child(note)
                    .test_support()
            })
            .child(card_footer(buttons, true))
            .test_support()
    }

    fn render_permission(
        &self,
        approval: &StoredApproval,
        tool: &str,
        input: &serde_json::Value,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = approval.id;
        let asker = self
            .state()
            .run(approval.run)
            .map_or_else(|| "An agent".to_string(), |r| self.name_of(r.agent));
        let wants = format!("{asker} wants to run {tool}");
        let text = input_text(input);
        let lines = text.lines().count();
        let open = self.open_inputs.contains(&id);
        let shown = if open || lines <= INPUT_LINES {
            text.clone()
        } else {
            text.lines()
                .take(INPUT_LINES)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let more = lines.saturating_sub(INPUT_LINES);
        needs_you_card(id, &wants)
            .child(mono_box(format!("approval-input-{id}"), shown))
            .when(more > 0 && !open, |card| {
                card.child(
                    div()
                        .id(format!("approval-input-more-{id}"))
                        .text_size(px(12.))
                        .text_color(color("accent-blue"))
                        .cursor_pointer()
                        .aria_label(format!("Show all ({more} more lines)"))
                        .child(format!("Show all ({more} more lines)"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_inputs.insert(id);
                            cx.notify();
                        }))
                        .test_support(),
                )
            })
            .child(card_footer(
                vec![
                    Button::new(format!("approval-allow-{id}"))
                        .primary()
                        .small()
                        .label("Allow once")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.decide(id, true, None, window, cx);
                        }))
                        .into_any_element(),
                    decline(id, "Deny", cx).into_any_element(),
                ],
                true,
            ))
            .test_support()
    }

    fn render_paused(
        &self,
        approval: &StoredApproval,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = approval.id;
        let thread: ThreadId = approval.thread;
        let state = self.state();
        let budget = state
            .thread(thread)
            .map(|t| {
                let channel = state
                    .channel(t.channel)
                    .map_or((None, None), |c| (c.handoff_policy, c.handoff_budget));
                effective(t, channel).1
            })
            .unwrap_or_default();
        let text = crew_core::store::paused_text(budget);
        indented(
            div()
                .id(format!("approval-{id}"))
                .flex()
                .flex_col()
                .gap(px(10.))
                .max_w(px(560.))
                .p(px(12.))
                .rounded(px(8.))
                .border_1()
                .border_color(color("border-200"))
                .aria_label(text.clone())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            Icon::new(IconName::Pause)
                                .size(px(14.))
                                .text_color(color("text-secondary")),
                        )
                        .child(text),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            Button::new(format!("approval-more-{id}"))
                                .outline()
                                .small()
                                .label(format!("Allow {MORE_HANDOFFS} more"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.request(
                                        Request::AllowMoreHandoffs {
                                            thread,
                                            n: MORE_HANDOFFS,
                                        },
                                        window,
                                        cx,
                                    );
                                    // The held-back handoff is the first of them.
                                    this.decide(id, true, None, window, cx);
                                })),
                        )
                        .child(
                            Button::new(format!("approval-end-{id}"))
                                .ghost()
                                .small()
                                .label("End thread")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.decide(id, false, None, window, cx);
                                    this.request(Request::StopThread { thread }, window, cx);
                                })),
                        ),
                )
                .test_support(),
        )
    }
}

fn decline(id: ApprovalId, label: &'static str, cx: &mut Context<Workspace>) -> Button {
    Button::new(format!("approval-decline-{id}"))
        .ghost()
        .small()
        .label(label)
        .on_click(cx.listener(move |this, _, window, cx| {
            this.decide(id, false, None, window, cx);
        }))
}

/// The amber frame and "Needs you · …" heading every pending card has.
fn needs_you_card(id: ApprovalId, wants: &str) -> Stateful<Div> {
    let label = format!("Needs you · {wants}");
    div()
        .id(format!("approval-{id}"))
        .flex()
        .flex_col()
        .gap(px(10.))
        .max_w(px(560.))
        .pt(px(12.))
        .px(px(12.))
        .rounded(px(8.))
        .border_1()
        .border_color(color("warning-border"))
        .bg(color("warning-subtle"))
        .aria_label(label)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size(px(14.))
                        .text_color(color("warning")),
                )
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(color("warning"))
                        .child("Needs you"),
                )
                .child(
                    div()
                        .text_color(color("text-primary"))
                        .child(format!("· {wants}")),
                ),
        )
}

fn mono_box(id: String, text: String) -> impl IntoElement {
    div()
        .id(id)
        .px(px(10.))
        .py(px(8.))
        .rounded(px(6.))
        .border_1()
        .border_color(color("border-200"))
        .bg(color("bg-100"))
        .font_family(mono_font())
        .text_size(px(12.))
        .aria_label(text.clone())
        .child(text)
        .test_support()
}

fn card_footer(buttons: Vec<AnyElement>, hint: bool) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .mx(px(-12.))
        .px(px(12.))
        .py(px(8.))
        .border_t_1()
        .border_color(color("warning-border"))
        .children(buttons)
        .child(div().flex_1())
        .when(hint, |footer| {
            footer.child(
                div()
                    .font_family(mono_font())
                    .text_size(px(11.))
                    .text_color(color("text-tertiary"))
                    .child("⌘↵ approve"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{decided_text, input_text};
    use crew_core::ids::{AgentId, ApprovalId, RunId, ThreadId};
    use crew_core::store::{ApprovalKind, ApprovalStatus, StoredApproval};

    #[test]
    fn inputs_show_their_command_or_json() {
        assert_eq!(
            input_text(&serde_json::json!({"command": "ls -la"})),
            "ls -la"
        );
        assert_eq!(
            input_text(&serde_json::json!({"file_path": "a"})),
            "{\n  \"file_path\": \"a\"\n}"
        );
    }

    #[test]
    fn decided_approvals_read_as_one_line() {
        let to = AgentId::new();
        let mut a = StoredApproval {
            id: ApprovalId::new(),
            thread: ThreadId::new(),
            run: RunId::new(),
            kind: ApprovalKind::Handoff {
                from: AgentId::new(),
                to,
                brief: "b".into(),
                paused: false,
            },
            status: ApprovalStatus::Approved,
            created_at: 0,
            decided_at: None,
        };
        let name = |_| "Scout".to_string();
        assert_eq!(decided_text(&a, name), "Handoff to Scout approved by you");
        a.status = ApprovalStatus::Declined;
        a.decided_at = Some(0);
        let at = crate::ui::common::clock(0);
        assert_eq!(
            decided_text(&a, name),
            format!("Handoff to Scout declined by you · {at}")
        );
        a.kind = ApprovalKind::Permission {
            tool: "Bash".into(),
            input: serde_json::Value::Null,
            tool_use_id: None,
        };
        a.status = ApprovalStatus::Approved;
        assert_eq!(
            decided_text(&a, name),
            format!("Bash allowed by you · {at}")
        );
    }
}
