//! Cards for what waits on you in a thread: a handoff to approve, a tool
//! to allow, a call the safety check stopped, or a used-up handoff
//! budget. Once decided, a card folds into one line.

use crew_core::agent::PermissionMode;
use crew_core::event::humanize_reason;
use crew_core::ids::{AgentId, ApprovalId, ThreadId};
use crew_core::store::{ApprovalKind, ApprovalStatus, StoredApproval};
use crew_proto::Request;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{clean_line, clean_lines, clock, monogram, runtime_badge};
use super::thread_settings::effective;
use super::thread_view::indented;
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::theme::color;

/// Input lines a permission card shows until you ask for all of it.
const INPUT_LINES: usize = 6;
/// Lines of the full command under a labelled ask.
const COMMAND_LINES: usize = 4;
/// How many more handoffs "Allow 3 more" lets run.
pub const MORE_HANDOFFS: u32 = 3;

/// A handoff brief being edited before approving.
pub(super) struct BriefEditor {
    pub(super) approval: ApprovalId,
    pub(super) input: Entity<TextareaState>,
}

/// The text a tool's input shows as: the files of a change list, else
/// crewd's label, else its command, else its JSON, pretty.
pub fn input_text(input: &serde_json::Value) -> String {
    clean_lines(&raw_input_text(input))
}

fn raw_input_text(input: &serde_json::Value) -> String {
    let files: Vec<&str> = input
        .get("changes")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("path").and_then(|p| p.as_str()))
        .collect();
    if !files.is_empty() {
        return files.join("\n");
    }
    if let Some(label) = text_field(input, "label") {
        return label.to_string();
    }
    match text_field(input, "command") {
        Some(command) => command.to_string(),
        None => serde_json::to_string_pretty(input).unwrap_or_default(),
    }
}

/// The full command behind a labelled ask, when the label shows less:
/// cleaned, and at most [`COMMAND_LINES`] lines, "…" marking a cut.
pub fn full_command(input: &serde_json::Value) -> Option<String> {
    let label = text_field(input, "label")?;
    let command = text_field(input, "command").filter(|c| *c != label)?;
    let lines: Vec<String> = command.lines().map(clean_line).collect();
    let mut shown = lines
        .iter()
        .take(COMMAND_LINES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > COMMAND_LINES {
        shown.push('…');
    }
    Some(shown)
}

fn text_field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
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
        (ApprovalKind::Blocked { .. }, ApprovalStatus::Approved) => {
            "Allowed and retried by you".to_string()
        }
        (ApprovalKind::Blocked { .. }, ApprovalStatus::Declined) => "Dismissed by you".to_string(),
        (ApprovalKind::Blocked { tool, .. }, _) => format!("{tool} no longer needed"),
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
    /// the whole window, so text fields don't take it. From the composer
    /// it approves handoffs only: a permission ask is never allowed by a
    /// reflex meant to send. A call the safety check stopped is only ever
    /// retried by its own button.
    pub(super) fn approve_latest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.nav.thread else {
            return;
        };
        let from_composer = self.composer.read(cx).focused(window, cx);
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
                .find(|a| match a.kind {
                    ApprovalKind::Handoff { paused, .. } => !paused,
                    ApprovalKind::Blocked { .. } => false,
                    ApprovalKind::Permission { .. } => !from_composer,
                })
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
            ApprovalKind::Blocked { input, reason, .. } => self
                .render_blocked(approval, input, reason, cx)
                .into_any_element(),
        }
    }

    /// A call Claude's safety check stopped. The run went on without it;
    /// "Allow and retry" starts a run that may make it once.
    fn render_blocked(
        &self,
        approval: &StoredApproval,
        input: &serde_json::Value,
        reason: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = approval.id;
        let agent = self
            .state()
            .run(approval.run)
            .map_or_else(|| "An agent".to_string(), |r| self.name_of(r.agent));
        let wants = format!("Claude's safety check stopped {agent}");
        let text = input_text(input);
        let shown = if text.lines().count() > INPUT_LINES {
            let mut cut = text
                .lines()
                .take(INPUT_LINES)
                .collect::<Vec<_>>()
                .join("\n");
            cut.push('…');
            cut
        } else {
            text
        };
        let reason = humanize_reason(reason);
        let deciding = self.deciding.contains(&id);
        let card = needs_you_card(id, &wants)
            .child(mono_box(format!("approval-input-{id}"), shown))
            .when(!reason.is_empty(), |card| {
                let line = format!("Reason: {reason}");
                card.child(
                    div()
                        .id(format!("approval-reason-{id}"))
                        .text_size(px(12.))
                        .text_color(color("text-secondary"))
                        .aria_label(line.clone())
                        .child(line)
                        .test_support(),
                )
            })
            .child(card_footer(
                vec![
                    Button::new(format!("approval-retry-{id}"))
                        .primary()
                        .small()
                        .label("Allow and retry")
                        .disabled(deciding)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.decide(id, true, None, window, cx);
                        }))
                        .into_any_element(),
                    decline(id, "Dismiss", cx)
                        .disabled(deciding)
                        .into_any_element(),
                ],
                false,
            ))
            .test_support();
        indented(card)
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
        let card = needs_you_card(id, &wants)
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
                    .when_some(target.map(|a| runtime_badge(&a.spec)), |row, badge| {
                        row.child(badge)
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
            .test_support();
        indented(card)
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
        let command = full_command(input);
        let card = needs_you_card(id, &wants)
            .child(mono_box(format!("approval-input-{id}"), shown))
            .when_some(command, |card, command| {
                card.child(
                    div()
                        .id(format!("approval-command-{id}"))
                        .truncate()
                        .font_family(mono_font())
                        .text_size(px(11.))
                        .text_color(color("text-tertiary"))
                        .aria_label(command.clone())
                        .child(command)
                        .test_support(),
                )
            })
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
            .test_support();
        indented(card)
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
        let deciding = self.deciding.contains(&id);
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
                                .disabled(deciding)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if this.deciding.contains(&id) {
                                        return;
                                    }
                                    // Approving the held-back handoff raises
                                    // the budget by one more, so it is the
                                    // first of the three.
                                    this.request(
                                        Request::AllowMoreHandoffs {
                                            thread,
                                            n: MORE_HANDOFFS - 1,
                                        },
                                        window,
                                        cx,
                                    );
                                    this.decide(id, true, None, window, cx);
                                })),
                        )
                        .child(
                            Button::new(format!("approval-end-{id}"))
                                .ghost()
                                .small()
                                .label("End thread")
                                .disabled(deciding)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if this.deciding.contains(&id) {
                                        return;
                                    }
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
    use super::{decided_text, full_command, input_text};
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
    fn codex_asks_show_their_label_or_files() {
        let bash = serde_json::json!({"command": "/bin/zsh -lc 'mkdir sub'", "cwd": "/work/demo", "label": "mkdir sub"});
        assert_eq!(input_text(&bash), "mkdir sub");
        assert_eq!(
            full_command(&bash).as_deref(),
            Some("/bin/zsh -lc 'mkdir sub'")
        );
        let plain = serde_json::json!({"command": "ls", "cwd": "/work/demo"});
        assert_eq!(input_text(&plain), "ls");
        assert_eq!(full_command(&plain).as_deref(), None);
        let access = serde_json::json!({"label": "network, write /work/demo/out", "cwd": "/work/demo", "permissions": {}});
        assert_eq!(input_text(&access), "network, write /work/demo/out");
        assert_eq!(full_command(&access).as_deref(), None);
        let edit = serde_json::json!({"path": "/work/demo/a.md, /work/demo/b.md", "changes": [
            {"path": "/work/demo/a.md", "diff": "x"}, {"path": "/work/demo/b.md", "diff": "y"}]});
        assert_eq!(input_text(&edit), "/work/demo/a.md\n/work/demo/b.md");
    }

    #[test]
    fn untrusted_ask_text_is_cleaned_and_clamped() {
        let sneaky = serde_json::json!({
            "command": "a\u{202e}b\r\nc\td\u{1b}[2J\nl3\nl4\nl5\nl6",
            "label": "x\u{2066}y\u{85}z",
        });
        assert_eq!(input_text(&sneaky), "xyz");
        assert_eq!(
            full_command(&sneaky).as_deref(),
            Some("ab\nc d[2J\nl3\nl4…")
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
        a.kind = ApprovalKind::Blocked {
            tool: "Bash".into(),
            input: serde_json::Value::Null,
            reason: String::new(),
        };
        assert_eq!(
            decided_text(&a, name),
            format!("Allowed and retried by you · {at}")
        );
        a.status = ApprovalStatus::Declined;
        assert_eq!(decided_text(&a, name), format!("Dismissed by you · {at}"));
    }
}
