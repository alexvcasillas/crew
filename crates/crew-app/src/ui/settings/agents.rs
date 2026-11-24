//! Settings › Agents: the list and the form that makes or changes one.

use crew_core::agent::{AgentSpec, DEFAULT_MAX_RUN_MINUTES, PermissionMode, Runtime};
use crew_core::ids::AgentId;
use crew_core::policy::HandoffPolicy;
use crew_proto::{Request, RuntimeInfo};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::runtimes::runtime_state;
use super::{control, field, form_error, help, segment, segmented};
use crate::fonts::mono_font;
use crate::theme::color;
use crate::ui::common::{chip, monogram, runtime_chip};
use crate::ui::workspace::{SIDEBAR_WIDTH, Workspace};

/// The models the segmented control offers, by the names `claude` takes.
pub const MODELS: [(&str, &str); 3] = [("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")];
/// What the instructions add to.
const INSTRUCTIONS_HELP: &str =
    "Added on top of the project's own CLAUDE.md or AGENTS.md, which still load.";
pub const CREDENTIALS: &str = "Your sign-ins stay with each CLI. Crew never reads, stores or sends a login token, and never calls a provider's API itself. It starts `claude` and reads what the CLI prints.";
pub const INSTALL_CLAUDE: &str = "Install Claude Code and run `claude auth login`.";
const COMING_SOON: &str = "Coming soon.";
/// Runtimes shown but not usable yet.
pub const COMING_RUNTIMES: [(&str, &str); 4] = [
    ("codex", "Codex"),
    ("grok", "Grok Build"),
    ("cursor", "Cursor"),
    ("acp", "Other ACP"),
];
const PERMISSIONS: [(PermissionMode, &str, &str); 2] = [
    (
        PermissionMode::EditFiles,
        "permission-edit",
        "Edit files, ask before commands",
    ),
    (
        PermissionMode::ReadOnly,
        "permission-read",
        "Read only (plan)",
    ),
];
/// The agent's own handoff setting. `None` leaves it to the channel.
const AGENT_POLICIES: [(Option<HandoffPolicy>, &str, &str); 3] = [
    (None, "agent-policy-channel", "Channel default"),
    (
        Some(HandoffPolicy::AskFirst),
        "agent-policy-ask",
        "Ask first",
    ),
    (
        Some(HandoffPolicy::Budget),
        "agent-policy-budget",
        "Run with budget",
    ),
];

/// Settings › Agents.
#[derive(Default)]
pub struct AgentsPage {
    pub form: Option<AgentForm>,
}

/// The agent being made or changed.
pub struct AgentForm {
    /// `None` for a new agent.
    pub editing: Option<AgentId>,
    pub name: Entity<InputState>,
    pub role: Entity<InputState>,
    /// One of [`MODELS`], used unless `other_model` holds a name.
    pub model: &'static str,
    pub other_model: Entity<InputState>,
    pub instructions: Entity<TextareaState>,
    pub permission: PermissionMode,
    pub allowed_tools: Entity<InputState>,
    pub policy: Option<HandoffPolicy>,
    pub budget: Entity<InputState>,
    pub max_minutes: Entity<InputState>,
    pub error: Option<String>,
    pub saving: bool,
    /// Saved, and nothing changed since.
    pub saved: bool,
    _edits: Vec<Subscription>,
}

/// What a new agent starts as.
pub fn blank_spec() -> AgentSpec {
    AgentSpec {
        name: String::new(),
        role: String::new(),
        runtime: Runtime::ClaudeCode,
        model: "sonnet".into(),
        instructions: String::new(),
        permission_mode: PermissionMode::EditFiles,
        allowed_tools: Vec::new(),
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: DEFAULT_MAX_RUN_MINUTES,
    }
}

/// The form's fields as text, before they are checked.
pub struct Draft<'a> {
    pub name: &'a str,
    pub role: &'a str,
    pub model: &'a str,
    pub other_model: &'a str,
    pub instructions: &'a str,
    pub permission: PermissionMode,
    pub allowed_tools: &'a str,
    pub policy: Option<HandoffPolicy>,
    pub budget: &'a str,
    pub max_minutes: &'a str,
}

/// Checks a draft against the names `taken` by other agents, giving the
/// spec to save or the sentence to show.
pub fn build_spec(d: &Draft<'_>, taken: &[&str]) -> Result<AgentSpec, String> {
    let name = d.name.trim();
    check_name(name, taken)?;
    let budget = match d.budget.trim() {
        "" => None,
        n => Some(
            n.parse::<u32>()
                .map_err(|_| "Handoffs per thread must be a whole number.".to_string())?,
        ),
    };
    let max_run_minutes = d
        .max_minutes
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or_else(|| "Longest run must be a whole number of minutes, 1 or more.".to_string())?;
    let other = d.other_model.trim();
    Ok(AgentSpec {
        name: name.to_string(),
        role: d.role.trim().to_string(),
        runtime: Runtime::ClaudeCode,
        model: if other.is_empty() { d.model } else { other }.to_string(),
        instructions: d.instructions.trim().to_string(),
        permission_mode: d.permission.clone(),
        allowed_tools: split_tools(d.allowed_tools),
        handoff_policy: d.policy,
        handoff_budget: budget,
        max_run_minutes,
    })
}

/// A name you can @mention and no other agent has.
pub fn check_name(name: &str, taken: &[&str]) -> Result<(), String> {
    if name.is_empty() {
        return Err("Give the agent a name.".into());
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "Use only letters, digits, - and _ in the name, so you can @mention it.".into(),
        );
    }
    if taken.iter().any(|t| t.eq_ignore_ascii_case(name)) {
        return Err(format!(
            "You already have an agent called {name}. Pick another name."
        ));
    }
    Ok(())
}

/// `Bash(cargo test:*), Read` as two tools. Commas inside brackets stay.
pub fn split_tools(text: &str) -> Vec<String> {
    let mut tools = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth <= 0 => {
                tools.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    tools.push(current);
    tools
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// What the app knows about the `claude` CLI.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeCheck {
    /// crewd hasn't said yet.
    Checking,
    NotInstalled,
    Installed {
        version: Option<String>,
        path: Option<String>,
        signed_in: Option<bool>,
    },
}

pub fn claude_check(runtimes: &[RuntimeInfo]) -> ClaudeCheck {
    if runtimes.is_empty() {
        return ClaudeCheck::Checking;
    }
    match runtimes.iter().find(|r| r.runtime == Runtime::ClaudeCode) {
        Some(r) if r.installed => ClaudeCheck::Installed {
            version: r.version.clone(),
            path: r.path.clone(),
            signed_in: r.signed_in,
        },
        _ => ClaudeCheck::NotInstalled,
    }
}

/// A form's fields as you typed them, valid or not.
#[derive(Clone)]
struct Fields {
    name: String,
    role: String,
    model: &'static str,
    other_model: String,
    instructions: String,
    permission: PermissionMode,
    allowed_tools: String,
    policy: Option<HandoffPolicy>,
    budget: String,
    max_minutes: String,
}

impl Fields {
    fn of(spec: &AgentSpec) -> Self {
        let known = MODELS.iter().find(|(m, _)| *m == spec.model).map(|m| m.0);
        Self {
            name: spec.name.clone(),
            role: spec.role.clone(),
            model: known.unwrap_or("sonnet"),
            other_model: if known.is_some() {
                String::new()
            } else {
                spec.model.clone()
            },
            instructions: spec.instructions.clone(),
            permission: spec.permission_mode.clone(),
            allowed_tools: spec.allowed_tools.join(", "),
            policy: spec.handoff_policy,
            budget: spec
                .handoff_budget
                .map(|b| b.to_string())
                .unwrap_or_default(),
            max_minutes: spec.max_run_minutes.to_string(),
        }
    }
}

impl AgentForm {
    fn new(
        editing: Option<AgentId>,
        f: Fields,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let mut input = |value: String, placeholder: &'static str, cx: &mut Context<Workspace>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let name = input(f.name, "Forge", cx);
        let role = input(f.role, "Engineer", cx);
        let other_model = input(f.other_model, "Other model, such as claude-sonnet-4-5", cx);
        let allowed_tools = input(
            f.allowed_tools,
            "e.g. Bash(cargo test:*), Bash(git diff:*)",
            cx,
        );
        let budget = input(f.budget, "3", cx);
        let max_minutes = input(f.max_minutes, "20", cx);
        let instructions = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder("What this agent is for and how it should work.")
                .default_value(f.instructions)
        });
        let edited = |this: &mut Workspace, event: &InputEvent, cx: &mut Context<Workspace>| {
            if matches!(event, InputEvent::Change) {
                if let Some(form) = this.agent_form_mut() {
                    form.saved = false;
                }
                cx.notify();
            }
        };
        let mut edits: Vec<Subscription> = [
            &name,
            &role,
            &other_model,
            &allowed_tools,
            &budget,
            &max_minutes,
        ]
        .into_iter()
        .map(|input| cx.subscribe(input, move |this, _, event, cx| edited(this, event, cx)))
        .collect();
        edits.push(cx.subscribe(&instructions, move |this, _, event, cx| {
            edited(this, event, cx)
        }));
        Self {
            editing,
            name,
            role,
            model: f.model,
            other_model,
            instructions,
            permission: f.permission,
            allowed_tools,
            policy: f.policy,
            budget,
            max_minutes,
            error: None,
            saving: false,
            saved: false,
            _edits: edits,
        }
    }

    /// What is in the form now.
    fn fields(&self, cx: &App) -> Fields {
        let text = |e: &Entity<InputState>| e.read(cx).value().to_string();
        Fields {
            name: text(&self.name),
            role: text(&self.role),
            model: self.model,
            other_model: text(&self.other_model),
            instructions: self.instructions.read(cx).value().to_string(),
            permission: self.permission.clone(),
            allowed_tools: text(&self.allowed_tools),
            policy: self.policy,
            budget: text(&self.budget),
            max_minutes: text(&self.max_minutes),
        }
    }

    /// Tells one form from another, so a late answer finds its own.
    fn id(&self) -> EntityId {
        self.name.entity_id()
    }

    fn spec(&self, taken: &[&str], cx: &App) -> Result<AgentSpec, String> {
        let f = self.fields(cx);
        build_spec(
            &Draft {
                name: &f.name,
                role: &f.role,
                model: f.model,
                other_model: &f.other_model,
                instructions: &f.instructions,
                permission: f.permission.clone(),
                allowed_tools: &f.allowed_tools,
                policy: f.policy,
                budget: &f.budget,
                max_minutes: &f.max_minutes,
            },
            taken,
        )
    }
}

impl Workspace {
    pub(super) fn agents_page_opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_form = self
            .settings
            .as_ref()
            .is_some_and(|s| s.agents.form.is_some());
        if has_form {
            return;
        }
        match self.state().agents().first().map(|a| a.id) {
            Some(first) => self.edit_agent(first, window, cx),
            None => self.new_agent(window, cx),
        }
    }

    fn agent_form_mut(&mut self) -> Option<&mut AgentForm> {
        self.settings.as_mut()?.agents.form.as_mut()
    }

    /// Opens an empty form.
    pub(crate) fn new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = AgentForm::new(None, Fields::of(&blank_spec()), window, cx);
        self.set_agent_form(form, window, cx);
    }

    pub(crate) fn edit_agent(
        &mut self,
        agent: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(spec) = self.state().agent(agent).map(|a| a.spec.clone()) else {
            return;
        };
        let form = AgentForm::new(Some(agent), Fields::of(&spec), window, cx);
        self.set_agent_form(form, window, cx);
    }

    /// A new form holding what this one holds, as typed, named
    /// `<Name>-copy`.
    fn duplicate_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.settings.as_ref().and_then(|s| s.agents.form.as_ref()) else {
            return;
        };
        let mut fields = form.fields(cx);
        fields.name = format!("{}-copy", fields.name.trim());
        let copy = AgentForm::new(None, fields, window, cx);
        self.set_agent_form(copy, window, cx);
    }

    fn set_agent_form(&mut self, form: AgentForm, window: &mut Window, cx: &mut Context<Self>) {
        let name = form.name.clone();
        if let Some(settings) = self.settings.as_mut() {
            settings.agents.form = Some(form);
        }
        // Only a new agent's name wants typing at once.
        let new = self
            .settings
            .as_ref()
            .and_then(|s| s.agents.form.as_ref())
            .is_some_and(|f| f.editing.is_none());
        if new {
            name.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    fn set_model(&mut self, model: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = self.agent_form_mut() {
            form.model = model;
            form.saved = false;
            form.other_model
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    fn set_permission(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        if let Some(form) = self.agent_form_mut() {
            form.permission = mode;
            form.saved = false;
        }
        cx.notify();
    }

    fn set_agent_policy(&mut self, policy: Option<HandoffPolicy>, cx: &mut Context<Self>) {
        if let Some(form) = self.agent_form_mut() {
            form.policy = policy;
            form.saved = false;
        }
        cx.notify();
    }

    /// Saves the form: a new agent, or the changes to one. Its next run
    /// uses them.
    pub(crate) fn save_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.claude_usable() {
            return;
        }
        let Some(form) = self.settings.as_ref().and_then(|s| s.agents.form.as_ref()) else {
            return;
        };
        if form.saving {
            return;
        }
        let editing = form.editing;
        let id = form.id();
        let taken: Vec<&str> = self
            .state()
            .agents()
            .into_iter()
            .filter(|a| Some(a.id) != editing)
            .map(|a| a.spec.name.as_str())
            .collect();
        let spec = form.spec(&taken, cx);
        let spec = match spec {
            Ok(spec) => spec,
            Err(error) => {
                if let Some(form) = self.agent_form_mut() {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        if let Some(form) = self.agent_form_mut() {
            form.error = None;
            form.saving = true;
        }
        let answer = self.client.request(match editing {
            Some(agent) => Request::UpdateAgent { agent, spec },
            None => Request::CreateAgent { spec },
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await;
            this.update(cx, |this, cx| {
                if let Some(form) = this.agent_form_mut().filter(|f| f.id() == id) {
                    form.saving = false;
                    match result {
                        Ok(value) => {
                            if form.editing.is_none() {
                                form.editing = serde_json::from_value(value).ok();
                            }
                            form.saved = true;
                        }
                        Err(e) => form.error = Some(e),
                    }
                }
                cx.notify();
            })
            .ok();
            Ok(())
        });
        cx.notify();
    }

    /// Asks before deleting the agent the form shows.
    fn confirm_delete_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self
            .settings
            .as_ref()
            .and_then(|s| s.agents.form.as_ref())
            .and_then(|f| f.editing)
        else {
            return;
        };
        let name = self
            .state()
            .agent(agent)
            .map_or_else(|| "this agent".to_string(), |a| a.spec.name.clone());
        let ask = format!("Delete {name}? Its messages and runs stay in their threads.");
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = this.clone();
            dialog
                .w(px(420.))
                .title("Delete agent")
                .child(
                    div()
                        .id("delete-agent-ask")
                        .text_color(color("text-secondary"))
                        .aria_label(ask.clone())
                        .child(ask.clone())
                        .test_support(),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("delete-agent-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("delete-agent-confirm")
                                .danger()
                                .label("Delete")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    confirm
                                        .update(cx, |this, cx| this.delete_agent(agent, window, cx))
                                        .ok();
                                }),
                        ),
                )
        });
    }

    fn delete_agent(&mut self, agent: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        let answer = self.client.request(Request::DeleteAgent { agent });
        self.spawn_request(window, cx, async move |this, cx| {
            answer.await.map_err(|e| (e, None))?;
            this.update(cx, |this, cx| {
                if let Some(settings) = this.settings.as_mut()
                    && settings.agents.form.as_ref().and_then(|f| f.editing) == Some(agent)
                {
                    settings.agents.form = None;
                }
                cx.notify();
            })
            .ok();
            Ok(())
        });
    }

    /// Whether agents can be saved: `claude` isn't known to be missing.
    fn claude_usable(&self) -> bool {
        claude_check(&self.runtimes) != ClaudeCheck::NotInstalled
    }

    pub(super) fn render_agents_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let form = self.settings.as_ref().and_then(|s| s.agents.form.as_ref());
        div()
            .flex()
            .size_full()
            .child(self.render_agent_list(form.and_then(|f| f.editing), cx))
            .child(match form {
                Some(form) => self.render_agent_form(form, cx).into_any_element(),
                None => div()
                    .flex_1()
                    .p(px(32.))
                    .child(help("Pick an agent, or make a new one."))
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn render_agent_list(
        &self,
        selected: Option<AgentId>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let agents = self.state().agents();
        let count = match agents.len() {
            1 => "1 agent".to_string(),
            n => format!("{n} agents"),
        };
        let rows: Vec<AnyElement> = agents
            .into_iter()
            .map(|a| {
                let id = a.id;
                div()
                    .id(SharedString::from(format!("settings-agent-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(44.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(selected == Some(id), |row| row.bg(color("surface-300")))
                    .hover(|row| row.bg(color("surface-200")))
                    .aria_label(a.spec.name.clone())
                    .child(monogram(&a.spec.name, 24.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(a.spec.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(color("text-secondary"))
                                    .child(a.spec.role.clone()),
                            ),
                    )
                    .child(chip(runtime_chip(&a.spec)))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.edit_agent(id, window, cx)),
                    )
                    .test_support()
                    .into_any_element()
            })
            .collect();
        div()
            .id("settings-agent-list")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .px(px(8.))
            .py(px(12.))
            .border_r_1()
            .border_color(color("border-100"))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .pl(px(8.))
                    .mb(px(4.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color("text-secondary"))
                            .child(count),
                    )
                    .child(
                        Button::new("settings-new-agent")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("New agent")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_agent(window, cx)),
                            ),
                    ),
            )
            .children(rows)
            .test_support()
    }

    fn render_agent_form(&self, form: &AgentForm, cx: &mut Context<Self>) -> impl IntoElement {
        let name = form.name.read(cx).value().to_string();
        let title = if form.editing.is_none() && name.trim().is_empty() {
            "New agent".to_string()
        } else {
            name.trim().to_string()
        };
        let usable = self.claude_usable();
        let other_set = !form.other_model.read(cx).value().trim().is_empty();
        let sub = match form.editing.and_then(|a| self.state().agent(a)) {
            Some(a) => runtime_chip(&a.spec),
            None => "Not saved yet".to_string(),
        };

        let models: Vec<AnyElement> = MODELS
            .iter()
            .map(|(value, label)| {
                let value: &'static str = value;
                segment(
                    format!("agent-model-{value}"),
                    label,
                    !other_set && form.model == value,
                )
                .on_click(cx.listener(move |this, _, window, cx| this.set_model(value, window, cx)))
                .test_support()
                .into_any_element()
            })
            .collect();
        let permission_label = PERMISSIONS
            .iter()
            .find(|(m, ..)| *m == form.permission)
            .map_or("", |(_, _, l)| l);
        let this = cx.entity().downgrade();
        let permission = Button::new("agent-permission")
            .outline()
            .small()
            .label(permission_label)
            .icon(IconName::ChevronDown)
            .dropdown_menu(move |menu, _, _| {
                PERMISSIONS.iter().fold(menu, |menu, (mode, _, label)| {
                    let this = this.clone();
                    let mode = mode.clone();
                    menu.item(PopupMenuItem::new(*label).on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| this.set_permission(mode.clone(), cx))
                            .ok();
                    }))
                })
            });
        let policies: Vec<AnyElement> = AGENT_POLICIES
            .iter()
            .map(|(p, id, label)| {
                let p = *p;
                segment(*id, label, form.policy == p)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_agent_policy(p, cx)))
                    .test_support()
                    .into_any_element()
            })
            .collect();

        div()
            .id("agent-form")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px(px(32.))
            .py(px(24.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(px(760.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .pb(px(16.))
                            .child(monogram(if title.is_empty() { "?" } else { &title }, 40.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .id("agent-form-title")
                                            .truncate()
                                            .text_size(px(24.))
                                            .line_height(px(32.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .aria_label(title.clone())
                                            .child(title.clone())
                                            .test_support(),
                                    )
                                    .child(help(sub)),
                            )
                            .when(form.saved, |row| {
                                row.child(
                                    div()
                                        .id("agent-saved")
                                        .text_size(px(12.))
                                        .text_color(color("success"))
                                        .aria_label("Saved")
                                        .child("Saved")
                                        .test_support(),
                                )
                            })
                            .when(form.editing.is_some(), |row| {
                                row.child(
                                    Button::new("agent-duplicate")
                                        .ghost()
                                        .small()
                                        .label("Duplicate")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.duplicate_agent(window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("agent-save")
                                    .primary()
                                    .small()
                                    .label("Save agent")
                                    .disabled(!usable || form.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_agent(window, cx);
                                    })),
                            ),
                    )
                    .when_some(form.error.clone(), |col, error| {
                        col.child(div().pb(px(8.)).child(form_error("agent-error", error)))
                    })
                    .child(field(
                        "Name and role",
                        Some("You @mention the agent by its name."),
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .child(control("agent-name", Input::new(&form.name))),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .child(control("agent-role", Input::new(&form.role))),
                            ),
                    ))
                    .child(field(
                        "Runtime",
                        Some("The CLI that runs this agent."),
                        self.render_runtime_cards(),
                    ))
                    .child(field(
                        "Model",
                        Some("Passed to the CLI as --model."),
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(segmented(models))
                            .child(control("agent-other-model", Input::new(&form.other_model))),
                    ))
                    .child(field(
                        "Instructions",
                        Some(INSTRUCTIONS_HELP),
                        div().font_family(mono_font()).child(control(
                            "agent-instructions",
                            Textarea::new(&form.instructions),
                        )),
                    ))
                    .child(field(
                        "What it can do",
                        Some("Edits stay in the thread's own worktree."),
                        div().flex().child(permission),
                    ))
                    .child(field(
                        "Always allow these tools",
                        Some("A comma list, passed as --allowedTools."),
                        control("agent-tools", Input::new(&form.allowed_tools)),
                    ))
                    .child(field(
                        "Bringing in other agents",
                        Some("When this agent mentions another one."),
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(segmented(policies))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .flex_1()
                                            .gap(px(4.))
                                            .child(help("Handoffs per thread"))
                                            .child(control(
                                                "agent-budget",
                                                Input::new(&form.budget),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .flex_1()
                                            .gap(px(4.))
                                            .child(help("Longest run, in minutes"))
                                            .child(control(
                                                "agent-max-minutes",
                                                Input::new(&form.max_minutes),
                                            )),
                                    ),
                            ),
                    ))
                    .child(
                        div()
                            .id("agent-credentials")
                            .mt(px(16.))
                            .p(px(12.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(color("accent-blue"))
                            .bg(color("surface-100"))
                            .text_size(px(12.))
                            .text_color(color("text-secondary"))
                            .aria_label(CREDENTIALS)
                            .child(CREDENTIALS)
                            .test_support(),
                    )
                    .when(form.editing.is_some(), |col| {
                        col.child(
                            div().mt(px(24.)).child(
                                Button::new("agent-delete")
                                    .outline()
                                    .small()
                                    .text_color(color("danger"))
                                    .label("Delete agent")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_delete_agent(window, cx);
                                    })),
                            ),
                        )
                    }),
            )
            .test_support()
    }

    /// Claude Code, then the runtimes that are coming.
    fn render_runtime_cards(&self) -> impl IntoElement {
        let info = self
            .runtimes
            .iter()
            .find(|r| r.runtime == Runtime::ClaudeCode);
        let (detail, status, token) = match (self.runtimes.is_empty(), info) {
            (true, _) => (String::new(), "Checking…", "text-tertiary"),
            (false, Some(info)) if info.installed => {
                let version = info.version.as_deref().unwrap_or("(version unknown)");
                let detail = [Some(format!("claude {version}")), info.path.clone()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                let (word, token) = runtime_state(info);
                (detail, word, token)
            }
            (false, info) => {
                let (word, token) = info.map_or(("Not installed", "text-tertiary"), runtime_state);
                (INSTALL_CLAUDE.to_string(), word, token)
            }
        };
        let claude = runtime_card("runtime-claude", true, true)
            .aria_label(format!("Claude Code {status}"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::MEDIUM).child("Claude Code"))
                    .when(!detail.is_empty(), |col| {
                        col.child(
                            div()
                                .id("runtime-claude-detail")
                                .text_size(px(12.))
                                .text_color(color("text-tertiary"))
                                .font_family(mono_font())
                                .aria_label(detail.clone())
                                .child(detail.clone())
                                .test_support(),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(color(token))
                    .child(status),
            )
            .test_support();
        let coming = COMING_RUNTIMES.iter().map(|(key, name)| {
            runtime_card(SharedString::from(format!("runtime-{key}")), false, false)
                .aria_label(format!("{name} Coming soon"))
                .child(div().flex_1().child(*name))
                .child(div().flex_none().text_size(px(12.)).child(COMING_SOON))
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(COMING_SOON).build(window, cx)
                })
                .test_support()
        });
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(claude)
            .children(coming)
    }
}

/// A radio card. Disabled cards are dimmed and do nothing.
fn runtime_card(id: impl Into<SharedString>, selected: bool, enabled: bool) -> Stateful<Div> {
    div()
        .id(ElementId::Name(id.into()))
        .flex()
        .items_center()
        .gap(px(10.))
        .min_h(px(44.))
        .px(px(12.))
        .py(px(8.))
        .rounded(px(8.))
        .border_1()
        .border_color(color(if selected {
            "accent-blue"
        } else {
            "border-200"
        }))
        .role(Role::RadioButton)
        .when(!enabled, |card| card.text_color(color("text-tertiary")))
        .child(
            div()
                .flex_none()
                .size(px(14.))
                .rounded_full()
                .border_1()
                .border_color(color(if selected {
                    "accent-blue"
                } else {
                    "border-200"
                }))
                .when(selected, |dot| {
                    dot.flex()
                        .items_center()
                        .justify_center()
                        .child(div().size(px(6.)).rounded_full().bg(color("accent-blue")))
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::{ClaudeCheck, Draft, build_spec, check_name, claude_check, split_tools};
    use crew_core::agent::{PermissionMode, Runtime};
    use crew_proto::RuntimeInfo;

    fn draft<'a>(name: &'a str) -> Draft<'a> {
        Draft {
            name,
            role: " Engineer ",
            model: "sonnet",
            other_model: "",
            instructions: "",
            permission: PermissionMode::EditFiles,
            allowed_tools: "Bash(cargo test:*), Bash(git diff:*)",
            policy: None,
            budget: "",
            max_minutes: "20",
        }
    }

    #[test]
    fn names_follow_the_mention_grammar_and_are_unique() {
        assert!(check_name("Forge", &[]).is_ok());
        assert!(check_name("code-review_2", &[]).is_ok());
        assert_eq!(check_name("", &[]).unwrap_err(), "Give the agent a name.");
        assert!(
            check_name("Forge copy", &[])
                .unwrap_err()
                .contains("letters, digits")
        );
        assert!(check_name("a@b", &[]).is_err());
        assert_eq!(
            check_name("forge", &["Forge"]).unwrap_err(),
            "You already have an agent called forge. Pick another name."
        );
    }

    #[test]
    fn a_draft_becomes_a_spec() {
        let spec = build_spec(&draft(" Forge "), &[]).unwrap();
        assert_eq!(spec.name, "Forge");
        assert_eq!(spec.role, "Engineer");
        assert_eq!(spec.model, "sonnet");
        assert_eq!(
            spec.allowed_tools,
            ["Bash(cargo test:*)", "Bash(git diff:*)"]
        );
        assert_eq!(spec.handoff_budget, None);
        assert_eq!(spec.max_run_minutes, 20);

        let mut other = draft("Forge");
        other.other_model = " claude-opus-4-1 ";
        other.budget = "5";
        assert_eq!(build_spec(&other, &[]).unwrap().model, "claude-opus-4-1");
        assert_eq!(build_spec(&other, &[]).unwrap().handoff_budget, Some(5));

        let mut zero = draft("Forge");
        zero.max_minutes = "0";
        assert!(
            build_spec(&zero, &[])
                .unwrap_err()
                .starts_with("Longest run")
        );
        let mut bad = draft("Forge");
        bad.budget = "three";
        assert!(
            build_spec(&bad, &[])
                .unwrap_err()
                .starts_with("Handoffs per thread")
        );
    }

    #[test]
    fn tools_split_on_commas_outside_brackets() {
        assert_eq!(
            split_tools("Bash(git diff:*), Read ,, Bash(a, b)"),
            ["Bash(git diff:*)", "Read", "Bash(a, b)"]
        );
        assert!(split_tools("  ").is_empty());
    }

    #[test]
    fn claude_reads_as_checking_missing_or_installed() {
        assert_eq!(claude_check(&[]), ClaudeCheck::Checking);
        let missing = RuntimeInfo {
            runtime: Runtime::ClaudeCode,
            installed: false,
            version: None,
            signed_in: None,
            path: None,
            models: vec![],
        };
        assert_eq!(
            claude_check(std::slice::from_ref(&missing)),
            ClaudeCheck::NotInstalled
        );
        let there = RuntimeInfo {
            installed: true,
            version: Some("2.1.0".into()),
            ..missing
        };
        assert!(matches!(
            claude_check(&[there]),
            ClaudeCheck::Installed { .. }
        ));
    }
}
