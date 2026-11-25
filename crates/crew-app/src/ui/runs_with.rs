//! Who runs the skill you picked: the chip at the start of the text, the
//! "Runs with" row under it, and its agent and model menus.

use crew_core::agent::Runtime;
use crew_proto::ModelInfo;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{codex_short, monogram, provider_icon, short_model};
use super::composer::{Composer, mentioned};
use super::mention_menu::MentionAgent;
use super::slash_menu::{SkillEntry, runtime_name};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// Under the agent menu.
pub const ONLY_RUNTIME: &str = "Only agents on the skill's runtime are listed.";
const MENU_GAP: f32 = 6.;
const AGENT_MENU_WIDTH: f32 = 320.;
const MODEL_MENU_WIDTH: f32 = 200.;
/// A down chevron, as drawn on the board.
const CHEVRON: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.75"><path d="M4 6l4 4 4-4"/></svg>"#;
/// A circle with a mark, for the wrong runtime note.
const ALERT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="8" cy="8" r="6"/><path d="M8 5v3.5M8 11v.5"/></svg>"#;

/// Who runs the skill.
#[derive(Debug, Clone, PartialEq)]
pub enum Target<'a> {
    /// An agent on the skill's runtime is mentioned: it runs it.
    Mentioned(&'a MentionAgent),
    /// No one is mentioned: the agent picked in the menu, if any is on
    /// the runtime.
    Picked(Option<&'a MentionAgent>),
    /// Only agents on the other runtime are mentioned. They stay
    /// mentioned; the picked agent runs the skill.
    WrongRuntime {
        mentioned: &'a MentionAgent,
        picked: Option<&'a MentionAgent>,
    },
}

/// What the "Runs with" row shows.
#[derive(Debug, Clone, PartialEq)]
pub struct RunsWith<'a> {
    pub runtime: Runtime,
    pub target: Target<'a>,
}

impl<'a> RunsWith<'a> {
    /// The agent that will run the skill, if there is one.
    pub fn agent(&self) -> Option<&'a MentionAgent> {
        match self.target {
            Target::Mentioned(agent) => Some(agent),
            Target::Picked(agent) | Target::WrongRuntime { picked: agent, .. } => agent,
        }
    }
}

/// Who runs `skill` for a message reading `text`. `choice` is the runtime
/// picked with the switch, `picked` the agent picked in the menu and
/// `reply_to` the agent a reply goes to.
pub fn resolve<'a>(
    skill: &SkillEntry,
    agents: &'a [MentionAgent],
    text: &str,
    choice: Option<&Runtime>,
    picked: Option<&str>,
    reply_to: Option<&str>,
) -> RunsWith<'a> {
    let mentions = mentioned(agents, text);
    let runtime = match choice {
        Some(r) if skill.has(r) => r.clone(),
        _ => mentions
            .iter()
            .map(|a| &a.runtime)
            .find(|r| skill.has(r))
            .unwrap_or(&skill.runtimes[0].0)
            .clone(),
    };
    if let Some(agent) = mentions.iter().find(|a| a.runtime == runtime) {
        return RunsWith {
            runtime,
            target: Target::Mentioned(agent),
        };
    }
    let on = |name: Option<&str>| {
        name.and_then(|n| agents.iter().find(|a| a.name == n && a.runtime == runtime))
    };
    let candidate = on(picked)
        .or_else(|| on(reply_to))
        .or_else(|| agents.iter().find(|a| a.runtime == runtime));
    let target = match mentions.first() {
        Some(mentioned) => Target::WrongRuntime {
            mentioned,
            picked: candidate,
        },
        None => Target::Picked(candidate),
    };
    RunsWith { runtime, target }
}

/// "Quill is on Codex. /review is a Claude skill."
pub fn wrong_runtime_note(mentioned: &MentionAgent, skill: &str, runtime: &Runtime) -> String {
    format!(
        "{} is on {}. /{skill} is a {} skill.",
        mentioned.name,
        runtime_name(&mentioned.runtime),
        runtime_name(runtime)
    )
}

/// A model as its chip names it: `sonnet`, `luna`.
pub fn model_name(runtime: &Runtime, id: &str, models: &[ModelInfo]) -> String {
    match runtime {
        Runtime::ClaudeCode => short_model(id).to_string(),
        Runtime::Codex => {
            let label = models.iter().find(|m| m.id == id).map(|m| m.label.as_str());
            codex_short(id, label)
        }
    }
}

/// `claude` or `codex`, as in a runtime chip.
pub fn runtime_word(runtime: &Runtime) -> &'static str {
    match runtime {
        Runtime::ClaudeCode => "claude",
        Runtime::Codex => "codex",
    }
}

/// `/unslop` on a chip, marked with the runtime it runs on.
pub fn skill_chip(id: impl Into<ElementId>, runtime: &Runtime, name: &str) -> Stateful<Div> {
    let name = format!("/{name}");
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(ui_px(5.))
        .h(scale::row(22.))
        .px(ui_px(6.))
        .border_1()
        .border_color(color("border-200"))
        .rounded(ui_px(4.))
        .bg(color("surface-300"))
        .font_family(code_family())
        .text_size(scale::mono(12.))
        .line_height(scale::mono_line(16.))
        .text_color(color("text-primary"))
        .aria_label(name.clone())
        .child(provider_icon(runtime, 11.))
        .child(name)
}

/// Which of the "Runs with" menus is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunsMenu {
    Agents,
    Models,
}

impl Composer {
    /// "Quill is on Codex. /review is a Claude skill."
    pub(super) fn render_runtime_note(&self, note: String) -> impl IntoElement {
        div()
            .id("runs-with-note")
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .px(ui_px(12.))
            .py(scale::row(8.))
            .border_t_1()
            .border_color(color("border-100"))
            .bg(color("warning-subtle"))
            .text_size(scale::text(12.))
            .text_color(color("warning"))
            .aria_label(note.clone())
            .child(Icon::default().data(ALERT).size(ui_px(13.)))
            .child(note)
            .test_support()
    }

    /// "Runs with", then who and on which model. `send` adds the Send
    /// button, for a reply.
    pub(super) fn render_runs_with(
        &self,
        skill: &SkillEntry,
        runs: &RunsWith,
        send: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let both = skill.both();
        let quiet_only = matches!(runs.target, Target::Picked(_)) && !both;
        let agent = runs.agent();
        let label = match (&runs.target, agent) {
            (Target::Mentioned(a), _) => format!("Runs with {} · mentioned", a.name),
            (_, Some(a)) => format!("Runs with {}", a.name),
            (_, None) => format!("Runs with no agent on {}", runtime_name(&runs.runtime)),
        };
        let mut row = div()
            .id("runs-with")
            .aria_label(label)
            .flex()
            .items_center()
            .gap(ui_px(6.))
            .h(scale::row(36.))
            .pl(ui_px(12.))
            .pr(ui_px(8.))
            .border_t_1()
            .border_color(color("border-100"))
            .text_size(scale::text(12.))
            .text_color(color("text-tertiary"))
            .whitespace_nowrap()
            .child("Runs with");
        if both {
            row = row.child(self.render_runtime_switch(&runs.runtime, cx));
        }
        match runs.target {
            Target::Mentioned(agent) => {
                row = row
                    .child(
                        div()
                            .id("runs-with-agent")
                            .text_color(color("text-primary"))
                            .aria_label(agent.name.clone())
                            .child(agent.name.clone())
                            .test_support(),
                    )
                    .child("· mentioned");
            }
            Target::Picked(picked) => {
                row = row.child(self.render_agent_picker(picked, &runs.runtime, !both, false, cx));
            }
            // The picker is edged in blue to point you at it.
            Target::WrongRuntime { picked, .. } => {
                row = row.child(self.render_agent_picker(picked, &runs.runtime, !both, true, cx));
            }
        }
        if let Some(agent) = agent {
            row = row.child(self.render_model_picker(agent, &runs.runtime, cx));
        }
        if quiet_only {
            row = row.child(
                div()
                    .id("runs-with-only")
                    .aria_label("this message only")
                    .child("this message only")
                    .test_support(),
            );
        }
        // The board draws Send in the picker and the switch states only.
        if send && (both || matches!(runs.target, Target::Picked(_))) {
            row = row.child(div().flex_1()).child(
                Button::new("composer-send")
                    .primary()
                    .small()
                    .label("Send")
                    .disabled(self.busy() || agent.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            );
        }
        row.test_support()
    }

    /// Claude | Codex, for a skill both have.
    fn render_runtime_switch(&self, picked: &Runtime, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |runtime: Runtime, cx: &mut Context<Self>| {
            let on = *picked == runtime;
            let name = runtime_name(&runtime);
            div()
                .id(SharedString::from(format!("runs-with-runtime-{name}")))
                .flex()
                .items_center()
                .gap(ui_px(5.))
                .h(scale::row(20.))
                .px(ui_px(8.))
                .rounded(ui_px(4.))
                .cursor_pointer()
                .font_weight(weight(FontWeight::MEDIUM))
                .text_color(color(if on { "text-primary" } else { "text-secondary" }))
                .when(on, |b| b.bg(color("surface-300")))
                .aria_label(name)
                .aria_selected(on)
                .child(provider_icon(&runtime, 11.))
                .child(name)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.choose_runtime(runtime.clone(), cx);
                }))
                .test_support()
        };
        div()
            .id("runs-with-runtime")
            .flex()
            .gap(ui_px(2.))
            .p(ui_px(2.))
            .border_1()
            .border_color(color("border-200"))
            .rounded(ui_px(6.))
            .aria_label("Runtime")
            .child(button(Runtime::ClaudeCode, cx))
            .child(button(Runtime::Codex, cx))
            .test_support()
    }

    /// The button that opens the agent menu, and the menu when open.
    fn render_agent_picker(
        &self,
        picked: Option<&MentionAgent>,
        runtime: &Runtime,
        show_runtime: bool,
        point: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.runs_menu == Some(RunsMenu::Agents);
        let label = picked.map_or_else(
            || format!("No agents on {}", runtime_name(runtime)),
            |a| a.name.clone(),
        );
        let button = picker_button("runs-with-agent", open, open || point)
            .aria_label(label.clone())
            .when_some(picked, |b, agent| {
                b.child(monogram(&agent.name, 16.).rounded(ui_px(4.)))
            })
            .child(label)
            .when(show_runtime, |b| {
                b.child(
                    div()
                        .font_family(code_family())
                        .text_size(scale::mono(11.))
                        .text_color(color("text-tertiary"))
                        .child(runtime_word(runtime)),
                )
            })
            .child(chevron())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_runs_menu(RunsMenu::Agents, cx)))
            .test_support();
        let menu = open.then(|| {
            let agents: Vec<&MentionAgent> = self
                .agents
                .iter()
                .filter(|a| a.runtime == *runtime)
                .collect();
            let rows: Vec<AnyElement> = agents
                .iter()
                .enumerate()
                .map(|(ix, agent)| {
                    let name = agent.name.clone();
                    let on = picked.is_some_and(|p| p.name == agent.name);
                    option(("runs-with-agent-option", ix), on)
                        .text_size(scale::text(13.))
                        .aria_label(agent.name.clone())
                        .child(monogram(&agent.name, 22.))
                        .child(div().flex_1().child(agent.name.clone()))
                        .child(super::common::chip(model_name(
                            &agent.runtime,
                            &agent.model,
                            &self.models_of(runtime),
                        )))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_agent(name.clone(), cx);
                        }))
                        .test_support()
                        .into_any_element()
                })
                .collect();
            let caption = format!("Agents on {}", runtime_name(runtime));
            self.popover("runs-with-agents", AGENT_MENU_WIDTH, rows.len(), cx)
                .aria_label(caption.clone())
                .child(caption_row(caption))
                .children(rows)
                .child(
                    div()
                        .id("runs-with-agents-note")
                        .aria_label(ONLY_RUNTIME)
                        .mt(scale::row(4.))
                        .px(ui_px(10.))
                        .pt(scale::row(6.))
                        .pb(scale::row(2.))
                        .border_t_1()
                        .border_color(color("border-100"))
                        .text_size(scale::text(11.))
                        .text_color(color("text-tertiary"))
                        .child(ONLY_RUNTIME)
                        .test_support(),
                )
                .test_support()
        });
        div()
            .relative()
            .child(button)
            .children(menu.map(|m| deferred(m).with_priority(2)))
    }

    /// The model chip: this message's model, and the runtime's models
    /// when open.
    fn render_model_picker(
        &self,
        agent: &MentionAgent,
        runtime: &Runtime,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.runs_menu == Some(RunsMenu::Models);
        let models = self.models_of(runtime);
        let current = self
            .model_choice
            .clone()
            .unwrap_or_else(|| agent.model.clone());
        let label = model_name(runtime, &current, &models);
        let button = picker_button("runs-with-model", open, open)
            .aria_label(label.clone())
            .child(label.clone())
            .child(chevron())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_runs_menu(RunsMenu::Models, cx)))
            .test_support();
        let menu = open.then(|| {
            let rows: Vec<AnyElement> = models
                .iter()
                .enumerate()
                .map(|(ix, model)| {
                    let name = model_name(runtime, &model.id, &models);
                    let id = model.id.clone();
                    option(("runs-with-model-option", ix), name == label)
                        .aria_label(name.clone())
                        .child(div().flex_1().child(model.label.clone()))
                        .child(super::common::chip(name))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_model(id.clone(), cx);
                        }))
                        .test_support()
                        .into_any_element()
                })
                .collect();
            self.popover("runs-with-models", MODEL_MENU_WIDTH, rows.len(), cx)
                .aria_label("Models")
                .children(rows)
                .test_support()
        });
        div()
            .relative()
            .child(button)
            .children(menu.map(|m| deferred(m).with_priority(2)))
    }

    /// A menu over or under the "Runs with" row, closed by a click
    /// elsewhere.
    fn popover(
        &self,
        id: &'static str,
        width: f32,
        rows: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let height = f32::from(scale::row(40. + 36. * rows as f32 + 30.));
        let below = self
            .top
            .get()
            .is_some_and(|top| super::slash_menu::opens_below(top, height));
        div()
            .id(id)
            .absolute()
            .left_0()
            .map(|m| {
                if below {
                    m.top_full().mt(ui_px(MENU_GAP))
                } else {
                    m.bottom_full().mb(ui_px(MENU_GAP))
                }
            })
            .w(ui_px(width))
            .flex()
            .flex_col()
            .p(ui_px(6.))
            .bg(color("surface-raised"))
            .border_1()
            .border_color(color("border-200"))
            .rounded(ui_px(8.))
            .shadow_lg()
            .role(Role::ListBox)
            .text_color(color("text-primary"))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_runs_menu(cx)))
    }

    fn models_of(&self, runtime: &Runtime) -> Vec<ModelInfo> {
        self.models
            .iter()
            .find(|(r, _)| r == runtime)
            .map(|(_, m)| m.clone())
            .unwrap_or_default()
    }
}

/// A button of the "Runs with" row; blue edged while its menu is open or
/// when it wants your attention.
fn picker_button(id: &'static str, open: bool, edged: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(ui_px(6.))
        .h(scale::row(24.))
        .pl(ui_px(8.))
        .pr(ui_px(6.))
        .border_1()
        .border_color(color(if edged { "accent-blue" } else { "border-200" }))
        .rounded(ui_px(6.))
        .bg(color("bg-100"))
        .text_color(color("text-primary"))
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .cursor_pointer()
        .aria_expanded(open)
}

fn chevron() -> impl IntoElement {
    Icon::default()
        .data(CHEVRON)
        .size(ui_px(10.))
        .text_color(color("text-secondary"))
}

fn caption_row(text: String) -> impl IntoElement {
    div()
        .px(ui_px(10.))
        .pt(scale::row(6.))
        .pb(scale::row(4.))
        .text_size(scale::text(11.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-tertiary"))
        .child(text.to_uppercase())
}

fn option(id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(ui_px(10.))
        .h(scale::row(36.))
        .px(ui_px(10.))
        .rounded(ui_px(6.))
        .text_size(scale::text(12.))
        .cursor_pointer()
        .when(selected, |row| row.bg(color("surface-300")))
        .hover(|row| row.bg(color("surface-200")))
        .role(Role::ListBoxOption)
        .aria_selected(selected)
}

#[cfg(test)]
mod tests {
    use super::{Target, resolve, wrong_runtime_note};
    use crate::model::ThreadStatus;
    use crate::ui::mention_menu::MentionAgent;
    use crate::ui::slash_menu::entries;
    use crew_core::agent::Runtime;
    use crew_core::ids::AgentId;
    use crew_proto::{SkillInfo, SkillSource};

    fn agent(name: &str, runtime: Runtime) -> MentionAgent {
        MentionAgent {
            id: AgentId::new(),
            name: name.into(),
            role: String::new(),
            chip: String::new(),
            model: "opus".into(),
            runtime,
            status: ThreadStatus::Idle,
        }
    }

    fn skill(runtimes: &[Runtime]) -> crate::ui::slash_menu::SkillEntry {
        let list: Vec<SkillInfo> = runtimes
            .iter()
            .map(|r| SkillInfo {
                runtime: r.clone(),
                name: "unslop".into(),
                description: String::new(),
                source: SkillSource::Global,
                path: "/s/SKILL.md".into(),
                shown: true,
            })
            .collect();
        entries(&list).remove(0)
    }

    fn name(t: &Target) -> Option<String> {
        match t {
            Target::Mentioned(a) => Some(format!("mentioned {}", a.name)),
            Target::Picked(a) => a.map(|a| format!("picked {}", a.name)),
            Target::WrongRuntime { mentioned, picked } => Some(format!(
                "wrong {} picked {}",
                mentioned.name,
                picked.map_or("none", |a| &a.name)
            )),
        }
    }

    #[test]
    fn the_skill_goes_to_an_agent_on_its_runtime() {
        let agents = [
            agent("Quill", Runtime::Codex),
            agent("Forge", Runtime::ClaudeCode),
            agent("Scout", Runtime::ClaudeCode),
        ];
        let claude = skill(&[Runtime::ClaudeCode]);
        let r = |text: &str, picked: Option<&str>, reply: Option<&str>| {
            name(&resolve(&claude, &agents, text, None, picked, reply).target).unwrap()
        };
        assert_eq!(r("@Scout the docs", None, None), "mentioned Scout");
        assert_eq!(r("the README", None, None), "picked Forge", "the first one");
        assert_eq!(r("the README", None, Some("Scout")), "picked Scout");
        assert_eq!(
            r("the README", Some("Scout"), Some("Forge")),
            "picked Scout"
        );
        assert_eq!(r("the README", Some("Quill"), None), "picked Forge");
        assert_eq!(r("@Quill the sync", None, None), "wrong Quill picked Forge");
        assert_eq!(
            wrong_runtime_note(&agents[0], "review", &Runtime::ClaudeCode),
            "Quill is on Codex. /review is a Claude skill."
        );

        // A skill both have follows the switch, else the mention.
        let both = skill(&[Runtime::ClaudeCode, Runtime::Codex]);
        let runs = resolve(&both, &agents, "@Quill go", None, None, None);
        assert_eq!(runs.runtime, Runtime::Codex);
        assert_eq!(name(&runs.target).unwrap(), "mentioned Quill");
        let runs = resolve(&both, &agents, "go", Some(&Runtime::Codex), None, None);
        assert_eq!(name(&runs.target).unwrap(), "picked Quill");
        let runs = resolve(&both, &agents, "go", None, None, None);
        assert_eq!(runs.runtime, Runtime::ClaudeCode);

        // No agent on the runtime: no one to run it.
        let codex = skill(&[Runtime::Codex]);
        let few = [agent("Forge", Runtime::ClaudeCode)];
        assert_eq!(resolve(&codex, &few, "go", None, None, None).agent(), None);
    }
}
