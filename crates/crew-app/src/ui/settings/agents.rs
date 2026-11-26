//! Settings › Agents: the list and the form that makes or changes one.

use crew_core::agent::{
    AgentSpec, DEFAULT_MAX_RUN_MINUTES, PermissionMode, Runtime, auto_supported,
};
use std::collections::HashMap;
use std::time::Duration;

use crew_core::ids::AgentId;
use crew_core::policy::HandoffPolicy;
use crew_proto::{ModelInfo, Request, RuntimeInfo};
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
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};
use crate::ui::common::{monogram, provider_icon, runtime_badge, runtime_chip};
use crate::ui::workspace::Workspace;

/// The models the segmented control offers, by the names `claude` takes.
pub const MODELS: [(&str, &str); 3] = [("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")];
/// What the instructions add to.
const INSTRUCTIONS_HELP: &str =
    "Added on top of the project's own CLAUDE.md or AGENTS.md, which still load.";
pub const CREDENTIALS: &str = "Your sign-ins stay with each CLI. Crew never reads, stores or sends a login token, and never calls a provider's API itself. It starts `claude` or `codex` and reads what the CLI prints.";
pub const INSTALL_CLAUDE: &str = "Install Claude Code and run `claude auth login`.";
pub const CODEX_LOGIN: &str = "Run `codex login` in a terminal.";
pub const INSTALL_CODEX: &str = "Install Codex to run agents with it.";
const COMING_SOON: &str = "Coming soon.";
/// Codex models up to this many show as a segmented control; more go in a
/// menu.
const MODEL_SEGMENTS: usize = 4;
/// Runtimes shown but not usable yet.
pub const COMING_RUNTIMES: [(&str, &str); 3] = [
    ("grok", "Grok Build"),
    ("cursor", "Cursor"),
    ("acp", "Other ACP"),
];
const PERMISSIONS: [(PermissionMode, &str, &str); 3] = [
    (
        PermissionMode::EditFiles,
        "permission-edit",
        "Edit files, ask before commands",
    ),
    (
        PermissionMode::Auto,
        "permission-auto",
        "Auto: edit files, ask only before risky commands",
    ),
    (
        PermissionMode::ReadOnly,
        "permission-read",
        "Read only (plan)",
    ),
];
/// What an agent with `mode` may do, as its settings word it.
pub fn permission_label(mode: PermissionMode) -> &'static str {
    PERMISSIONS
        .iter()
        .find(|(m, ..)| *m == mode)
        .map_or("", |(_, _, label)| label)
}

/// The longest run choices, in minutes. 0 is no limit.
pub const RUN_LIMITS: [u32; 5] = [0, 30, 60, 120, 240];

/// How a run limit reads: `No limit`, `30 min`, `1 h`.
pub fn run_limit_label(minutes: u32) -> String {
    match minutes {
        0 => "No limit".into(),
        m if m % 60 == 0 => format!("{} h", m / 60),
        m => format!("{m} min"),
    }
}

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

/// How long typing pauses before an existing agent's text is saved.
pub const AUTOSAVE_PAUSE: Duration = Duration::from_millis(600);

/// Saves of existing agents, one at a time, so the last edit lands last.
#[derive(Default)]
pub struct AgentSaves {
    /// A save is in flight.
    busy: bool,
    /// Edits waiting for the save in flight, in order, at most one per
    /// agent: a newer edit takes the place of its agent's older one.
    queued: Vec<AgentSave>,
    /// The newest spec sent or queued per agent, until crewd's copy
    /// matches it. A reopened form starts from it.
    latest: HashMap<AgentId, AgentSpec>,
    task: Option<Task<()>>,
}

impl AgentSaves {
    /// Nothing in flight and nothing waiting.
    pub fn idle(&self) -> bool {
        !self.busy && self.queued.is_empty()
    }

    /// How many edits wait for the save in flight.
    pub fn queued(&self) -> usize {
        self.queued.len()
    }

    fn queue(&mut self, save: AgentSave) {
        match self.queued.iter_mut().find(|q| q.agent == save.agent) {
            Some(older) => *older = save,
            None => self.queued.push(save),
        }
    }
}

/// One agent's spec to save, and the form it came from.
struct AgentSave {
    agent: AgentId,
    spec: AgentSpec,
    form: EntityId,
}

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
    pub runtime: Runtime,
    /// One of the runtime's models, used unless `other_model` holds a
    /// name. Empty for a Codex agent before crewd listed its models: the
    /// default one is then used.
    pub model: String,
    pub other_model: Entity<InputState>,
    pub instructions: Entity<TextareaState>,
    pub permission: PermissionMode,
    pub allowed_tools: Entity<InputState>,
    pub policy: Option<HandoffPolicy>,
    pub budget: Entity<InputState>,
    /// The longest a run may take, in minutes. 0 is no limit.
    pub max_minutes: u32,
    pub error: Option<String>,
    pub saving: bool,
    /// Saved, and nothing changed since.
    pub saved: bool,
    /// The last spec sent for an existing agent, so the same one isn't
    /// sent twice.
    sent: Option<AgentSpec>,
    /// Saves typed text once typing pauses.
    debounce: Option<Task<()>>,
    _edits: Vec<Subscription>,
}

/// Said when a Claude model can't use Auto.
pub const AUTO_NEEDS: &str = "Auto needs Opus 4.6, Sonnet 4.6, Haiku 5.5 or newer.";

/// Whether an agent of `runtime` on `model` may use Auto. Codex always can.
pub fn auto_allowed(runtime: &Runtime, model: &str) -> bool {
    *runtime == Runtime::Codex || auto_supported(model)
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
    pub runtime: Runtime,
    pub model: &'a str,
    pub other_model: &'a str,
    pub instructions: &'a str,
    pub permission: PermissionMode,
    pub allowed_tools: &'a str,
    pub policy: Option<HandoffPolicy>,
    pub budget: &'a str,
    pub max_minutes: u32,
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
    let max_run_minutes = d.max_minutes;
    let other = d.other_model.trim();
    let model = if other.is_empty() { d.model } else { other };
    if d.permission == PermissionMode::Auto && !auto_allowed(&d.runtime, model) {
        return Err(AUTO_NEEDS.into());
    }
    Ok(AgentSpec {
        name: name.to_string(),
        role: d.role.trim().to_string(),
        runtime: d.runtime.clone(),
        model: model.to_string(),
        instructions: d.instructions.trim().to_string(),
        permission_mode: d.permission.clone(),
        // Only `claude` takes a list of tools to allow.
        allowed_tools: match d.runtime {
            Runtime::ClaudeCode => split_tools(d.allowed_tools),
            Runtime::Codex => Vec::new(),
        },
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

/// The Codex model a new agent starts on: the first Luna model, else the
/// first listed. Empty if none is listed, which leaves it to Codex.
pub fn default_codex_model(models: &[ModelInfo]) -> String {
    crew_proto::default_codex_model(models).map_or_else(String::new, |m| m.id.clone())
}

/// The models crewd listed for `runtime`.
pub fn models_of<'a>(runtimes: &'a [RuntimeInfo], runtime: &Runtime) -> &'a [ModelInfo] {
    runtimes
        .iter()
        .find(|r| r.runtime == *runtime)
        .map_or(&[], |r| r.models.as_slice())
}

/// Whether an agent on `runtime` can be saved: Claude Code isn't known to
/// be missing, or Codex is installed and not known to be signed out.
pub fn runtime_usable(runtime: &Runtime, runtimes: &[RuntimeInfo]) -> bool {
    match runtime {
        Runtime::ClaudeCode => claude_check(runtimes) != ClaudeCheck::NotInstalled,
        Runtime::Codex => runtimes
            .iter()
            .find(|r| r.runtime == Runtime::Codex)
            .is_some_and(|r| r.installed && r.signed_in != Some(false)),
    }
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
    runtime: Runtime,
    model: String,
    other_model: String,
    instructions: String,
    permission: PermissionMode,
    allowed_tools: String,
    policy: Option<HandoffPolicy>,
    budget: String,
    max_minutes: u32,
}

impl Fields {
    /// The fields of `spec`, with `runtimes` telling which Codex models
    /// are known.
    fn of(spec: &AgentSpec, runtimes: &[RuntimeInfo]) -> Self {
        let known = match spec.runtime {
            Runtime::ClaudeCode => MODELS.iter().any(|(m, _)| *m == spec.model),
            Runtime::Codex => {
                let models = models_of(runtimes, &Runtime::Codex);
                // Not listed yet: kept as the pick, not as another model.
                models.is_empty() || models.iter().any(|m| m.id == spec.model)
            }
        };
        let model = match (&spec.runtime, known) {
            (_, true) => spec.model.clone(),
            (Runtime::ClaudeCode, false) => "sonnet".into(),
            (Runtime::Codex, false) => default_codex_model(models_of(runtimes, &Runtime::Codex)),
        };
        Self {
            name: spec.name.clone(),
            role: spec.role.clone(),
            runtime: spec.runtime.clone(),
            model,
            other_model: if known {
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
            max_minutes: spec.max_run_minutes,
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
        let instructions = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder("What this agent is for and how it should work.")
                .default_value(f.instructions)
        });
        let edited =
            |this: &mut Workspace, event: &InputEvent, cx: &mut Context<Workspace>| match event {
                InputEvent::Change => this.agent_changed(true, cx),
                InputEvent::Blur => this.flush_agent_save(cx),
                _ => {}
            };
        let mut edits: Vec<Subscription> = [&name, &role, &other_model, &allowed_tools, &budget]
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
            runtime: f.runtime,
            model: f.model,
            other_model,
            instructions,
            permission: f.permission,
            allowed_tools,
            policy: f.policy,
            budget,
            max_minutes: f.max_minutes,
            error: None,
            saving: false,
            // An existing agent opens as saved.
            saved: editing.is_some(),
            sent: None,
            debounce: None,
            _edits: edits,
        }
    }

    /// What is in the form now.
    fn fields(&self, cx: &App) -> Fields {
        let text = |e: &Entity<InputState>| e.read(cx).value().to_string();
        Fields {
            name: text(&self.name),
            role: text(&self.role),
            runtime: self.runtime.clone(),
            model: self.model.clone(),
            other_model: text(&self.other_model),
            instructions: self.instructions.read(cx).value().to_string(),
            permission: self.permission.clone(),
            allowed_tools: text(&self.allowed_tools),
            policy: self.policy,
            budget: text(&self.budget),
            max_minutes: self.max_minutes,
        }
    }

    /// Tells one form from another, so a late answer finds its own.
    fn id(&self) -> EntityId {
        self.name.entity_id()
    }

    /// The spec to save. A Codex agent with no model picked yet gets the
    /// default of `models`.
    fn spec(&self, taken: &[&str], models: &[ModelInfo], cx: &App) -> Result<AgentSpec, String> {
        let f = self.fields(cx);
        let model = match (&f.runtime, f.model.is_empty()) {
            (Runtime::Codex, true) => default_codex_model(models),
            _ => f.model.clone(),
        };
        build_spec(
            &Draft {
                name: &f.name,
                role: &f.role,
                runtime: f.runtime.clone(),
                model: &model,
                other_model: &f.other_model,
                instructions: &f.instructions,
                permission: f.permission.clone(),
                allowed_tools: &f.allowed_tools,
                policy: f.policy,
                budget: &f.budget,
                max_minutes: f.max_minutes,
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
        let form = AgentForm::new(None, Fields::of(&blank_spec(), &self.runtimes), window, cx);
        self.set_agent_form(form, window, cx);
    }

    pub(crate) fn edit_agent(
        &mut self,
        agent: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self
            .settings
            .as_ref()
            .and_then(|s| s.agents.form.as_ref())
            .is_some_and(|f| f.editing == Some(agent));
        if open {
            return;
        }
        // What you typed in the open form goes first, so a form for the
        // same agent starts from it.
        self.flush_agent_save(cx);
        let Some(stored) = self.state().agent(agent).map(|a| a.spec.clone()) else {
            return;
        };
        // crewd may not have caught up with the newest save yet.
        let pending = match self.agent_saves.latest.get(&agent) {
            Some(latest) if *latest == stored => {
                self.agent_saves.latest.remove(&agent);
                None
            }
            latest => latest.cloned(),
        };
        let spec = pending.clone().unwrap_or(stored);
        let mut form = AgentForm::new(Some(agent), Fields::of(&spec, &self.runtimes), window, cx);
        form.sent = pending;
        self.set_agent_form(form, window, cx);
    }

    /// Focuses the text control of the open form's `field`, if it has
    /// one. Runtime and What it can do are cards and a menu: they keep
    /// focus where it was.
    pub(super) fn focus_agent_field(
        &mut self,
        field: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.settings.as_ref().and_then(|s| s.agents.form.as_ref()) else {
            return;
        };
        let input = match field {
            "field-agent-name" => &form.name,
            "field-agent-model" => &form.other_model,
            "field-agent-tools" => &form.allowed_tools,
            "field-agent-handoffs" => &form.budget,
            "field-agent-instructions" => {
                let textarea = form.instructions.clone();
                textarea.update(cx, |t, cx| t.focus(window, cx));
                return;
            }
            _ => return,
        };
        input.clone().update(cx, |i, cx| i.focus(window, cx));
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
        self.flush_agent_save(cx);
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

    fn set_model(&mut self, model: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = self.agent_form_mut() {
            form.model = model;
            form.other_model
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.agent_changed(false, cx);
    }

    /// Picks the runtime, starting on its default model.
    fn set_runtime(&mut self, runtime: Runtime, window: &mut Window, cx: &mut Context<Self>) {
        let model = match runtime {
            Runtime::ClaudeCode => "sonnet".to_string(),
            Runtime::Codex => default_codex_model(models_of(&self.runtimes, &Runtime::Codex)),
        };
        let Some(form) = self.agent_form_mut().filter(|f| f.runtime != runtime) else {
            return;
        };
        form.runtime = runtime;
        self.set_model(model, window, cx);
    }

    fn set_permission(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        if let Some(form) = self.agent_form_mut() {
            form.permission = mode;
        }
        self.agent_changed(false, cx);
    }

    fn set_max_minutes(&mut self, minutes: u32, cx: &mut Context<Self>) {
        if let Some(form) = self.agent_form_mut() {
            form.max_minutes = minutes;
        }
        self.agent_changed(false, cx);
    }

    fn set_agent_policy(&mut self, policy: Option<HandoffPolicy>, cx: &mut Context<Self>) {
        if let Some(form) = self.agent_form_mut() {
            form.policy = policy;
        }
        self.agent_changed(false, cx);
    }

    /// Saves the form: a new agent, or the changes to one. Its next run
    /// uses them.
    pub(crate) fn save_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.settings.as_ref().and_then(|s| s.agents.form.as_ref()) else {
            return;
        };
        if form.saving || !runtime_usable(&form.runtime, &self.runtimes) {
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
        let spec = form.spec(&taken, models_of(&self.runtimes, &Runtime::Codex), cx);
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

    /// Something in the form changed. An existing agent saves it now, or
    /// after a pause when you're `typing`. A new agent waits for its
    /// button.
    pub(crate) fn agent_changed(&mut self, typing: bool, cx: &mut Context<Self>) {
        let Some(form) = self.agent_form_mut() else {
            return;
        };
        form.saved = false;
        if form.editing.is_none() {
            cx.notify();
            return;
        }
        if typing {
            form.debounce = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(AUTOSAVE_PAUSE).await;
                this.update(cx, |this, cx| this.autosave_agent(cx)).ok();
            }));
            cx.notify();
        } else {
            self.autosave_agent(cx);
        }
    }

    /// Saves typed text that is still waiting for its pause: on blur, and
    /// before the form goes away.
    pub(crate) fn flush_agent_save(&mut self, cx: &mut Context<Self>) {
        let waiting = self
            .settings
            .as_ref()
            .and_then(|s| s.agents.form.as_ref())
            .is_some_and(|f| f.debounce.is_some());
        if waiting {
            self.autosave_agent(cx);
        }
    }

    /// Saves the existing agent the form shows, if what it holds is
    /// valid. A sentence says why not otherwise.
    fn autosave_agent(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.settings.as_ref().and_then(|s| s.agents.form.as_ref()) else {
            return;
        };
        let Some(agent) = form.editing else {
            return;
        };
        let id = form.id();
        let taken: Vec<&str> = self
            .state()
            .agents()
            .into_iter()
            .filter(|a| a.id != agent)
            .map(|a| a.spec.name.as_str())
            .collect();
        let spec = form.spec(&taken, models_of(&self.runtimes, &Runtime::Codex), cx);
        let Some(form) = self.agent_form_mut() else {
            return;
        };
        form.debounce = None;
        cx.notify();
        // Saved whatever the runtime's state: crewd stores the spec
        // without the CLI, and the runtime card says what it needs.
        let spec = match spec {
            Ok(spec) => spec,
            Err(error) => {
                form.error = Some(error);
                return;
            }
        };
        form.error = None;
        if form.sent.as_ref() == Some(&spec) {
            // Already sent; its answer says how it went.
            form.saved = !form.saving;
            return;
        }
        form.sent = Some(spec.clone());
        form.saving = true;
        self.send_agent_save(
            AgentSave {
                agent,
                spec,
                form: id,
            },
            cx,
        );
    }

    /// Sends `save` now, or after the save in flight, in place of any
    /// edit already waiting.
    fn send_agent_save(&mut self, save: AgentSave, cx: &mut Context<Self>) {
        self.agent_saves
            .latest
            .insert(save.agent, save.spec.clone());
        if self.agent_saves.busy {
            self.agent_saves.queue(save);
            return;
        }
        self.agent_saves.busy = true;
        let answer = self.client.request(Request::UpdateAgent {
            agent: save.agent,
            spec: save.spec.clone(),
        });
        self.agent_saves.task = Some(cx.spawn(async move |this, cx| {
            let result = answer.await;
            this.update(cx, |this, cx| this.agent_save_answered(save, result, cx))
                .ok();
        }));
    }

    /// Drops the newest spec kept for an agent once crewd's copy holds it
    /// and no save for it is waiting, so a change made elsewhere later
    /// isn't hidden behind it.
    pub(crate) fn forget_landed_specs(&mut self) {
        let saves = &self.agent_saves;
        let landed: Vec<AgentId> = saves
            .latest
            .iter()
            .filter(|(agent, spec)| {
                !saves.queued.iter().any(|q| q.agent == **agent)
                    && self
                        .state()
                        .agent(**agent)
                        .is_some_and(|a| a.spec == **spec)
            })
            .map(|(agent, _)| *agent)
            .collect();
        for agent in landed {
            self.agent_saves.latest.remove(&agent);
        }
    }

    /// crewd answered `save`. The next edit waiting, if any, goes next.
    fn agent_save_answered(
        &mut self,
        save: AgentSave,
        result: Result<serde_json::Value, String>,
        cx: &mut Context<Self>,
    ) {
        self.agent_saves.busy = false;
        let more = self.agent_saves.queued.iter().any(|q| q.form == save.form);
        if result.is_err() && self.agent_saves.latest.get(&save.agent) == Some(&save.spec) {
            // Refused: crewd's copy stays the one to start from.
            self.agent_saves.latest.remove(&save.agent);
        }
        self.forget_landed_specs();
        if let Some(f) = self
            .agent_form_mut()
            .filter(|f| f.id() == save.form && !more)
        {
            f.saving = false;
            match result {
                Ok(_) => f.saved = f.debounce.is_none(),
                Err(e) => {
                    f.error = Some(e);
                    f.sent = None;
                }
            }
        }
        if !self.agent_saves.queued.is_empty() {
            let next = self.agent_saves.queued.remove(0);
            self.send_agent_save(next, cx);
        }
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
                .w(ui_px(420.))
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
                    .p(ui_px(32.))
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
                    .gap(ui_px(10.))
                    .h(scale::row(44.))
                    .px(ui_px(8.))
                    .rounded(ui_px(6.))
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
                                    .font_weight(weight(FontWeight::MEDIUM))
                                    .child(a.spec.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(scale::text(12.))
                                    .text_color(color("text-secondary"))
                                    .child(a.spec.role.clone()),
                            ),
                    )
                    .child({
                        let text = runtime_chip(&a.spec);
                        runtime_badge(&a.spec)
                            .id(SharedString::from(format!("settings-agent-chip-{id}")))
                            .aria_label(text)
                            .test_support()
                    })
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
            .w(self.sidebar_width())
            .h_full()
            .px(ui_px(8.))
            .py(scale::row(12.))
            .border_r_1()
            .border_color(color("border-100"))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(scale::row(28.))
                    .pl(ui_px(8.))
                    .mb(ui_px(4.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(scale::text(12.))
                            .font_weight(weight(FontWeight::MEDIUM))
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
        let jump = self.jump(cx);
        let name = form.name.read(cx).value().to_string();
        let title = if form.editing.is_none() && name.trim().is_empty() {
            "New agent".to_string()
        } else {
            name.trim().to_string()
        };
        let usable = runtime_usable(&form.runtime, &self.runtimes);
        let other_set = !form.other_model.read(cx).value().trim().is_empty();
        let sub = match form.editing.and_then(|a| self.state().agent(a)) {
            Some(a) => {
                let text = runtime_chip(&a.spec);
                runtime_badge(&a.spec)
                    .id("agent-form-chip")
                    .aria_label(text)
                    .test_support()
                    .into_any_element()
            }
            None => help("Not saved yet").into_any_element(),
        };
        let models = self.render_model_picker(form, other_set, cx);
        let permission_label = PERMISSIONS
            .iter()
            .find(|(m, ..)| *m == form.permission)
            .map_or("", |(_, _, l)| l);
        let model = form.other_model.read(cx).value().trim().to_string();
        let model = if model.is_empty() {
            form.model.clone()
        } else {
            model
        };
        let auto_ok = auto_allowed(&form.runtime, &model);
        let this = cx.entity().downgrade();
        let permission = Button::new("agent-permission")
            .outline()
            .small()
            .label(permission_label)
            .icon(IconName::ChevronDown)
            .dropdown_menu(move |menu, _, _| {
                PERMISSIONS.iter().fold(menu, |menu, (mode, _, label)| {
                    let this = this.clone();
                    let disabled = *mode == PermissionMode::Auto && !auto_ok;
                    let mode = mode.clone();
                    menu.item(PopupMenuItem::new(*label).disabled(disabled).on_click(
                        move |_, _, cx| {
                            this.update(cx, |this, cx| this.set_permission(mode.clone(), cx))
                                .ok();
                        },
                    ))
                })
            });
        // A limit set another way, such as from the CLI, is offered too.
        let mut limits = RUN_LIMITS.to_vec();
        if !limits.contains(&form.max_minutes) {
            limits.push(form.max_minutes);
            limits.sort_unstable();
        }
        let limit_label = run_limit_label(form.max_minutes);
        let this = cx.entity().downgrade();
        let max_minutes = Button::new("agent-max-minutes")
            .outline()
            .small()
            .label(limit_label.clone())
            .icon(IconName::ChevronDown)
            .dropdown_menu(move |menu, _, _| {
                limits.iter().fold(menu, |menu, minutes| {
                    let this = this.clone();
                    let minutes = *minutes;
                    menu.item(PopupMenuItem::new(run_limit_label(minutes)).on_click(
                        move |_, _, cx| {
                            this.update(cx, |this, cx| this.set_max_minutes(minutes, cx))
                                .ok();
                        },
                    ))
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
            .track_scroll(jump.scroll())
            .px(ui_px(32.))
            .py(ui_px(24.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(ui_px(760.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(12.))
                            .pb(scale::row(16.))
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
                                            .text_size(scale::text(24.))
                                            .line_height(scale::line(32.))
                                            .font_weight(weight(FontWeight::SEMIBOLD))
                                            .aria_label(title.clone())
                                            .child(title.clone())
                                            .test_support(),
                                    )
                                    .child(div().flex().child(sub)),
                            )
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
                            // An existing agent saves as you change it.
                            .when(form.editing.is_none(), |row| {
                                row.child(
                                    Button::new("agent-save")
                                        .primary()
                                        .small()
                                        .label("Save agent")
                                        .disabled(!usable || form.saving)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.save_agent(window, cx);
                                        })),
                                )
                            }),
                    )
                    .when_some(form.error.clone(), |col, error| {
                        col.child(
                            div()
                                .pb(scale::row(8.))
                                .child(form_error("agent-error", error)),
                        )
                    })
                    .child(field(
                        &jump,
                        "field-agent-name",
                        "Name and role",
                        Some("You @mention the agent by its name."),
                        div()
                            .flex()
                            .gap(ui_px(8.))
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
                        &jump,
                        "field-agent-runtime",
                        "Runtime",
                        Some("The CLI that runs this agent."),
                        self.render_runtime_cards(&form.runtime, cx),
                    ))
                    .child(field(
                        &jump,
                        "field-agent-model",
                        "Model",
                        Some("Passed to the CLI as --model."),
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(6.))
                            .child(models)
                            .child(control("agent-other-model", Input::new(&form.other_model))),
                    ))
                    .child(field(
                        &jump,
                        "field-agent-instructions",
                        "Instructions",
                        Some(INSTRUCTIONS_HELP),
                        div().font_family(code_family()).child(control(
                            "agent-instructions",
                            Textarea::new(&form.instructions),
                        )),
                    ))
                    .child(field(
                        &jump,
                        "field-agent-permission",
                        "What it can do",
                        Some("Edits stay in the thread's own worktree."),
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(6.))
                            .child(div().flex().child(permission))
                            .when(!auto_ok, |col| {
                                col.child(
                                    div()
                                        .id("agent-auto-needs")
                                        .aria_label(AUTO_NEEDS)
                                        .child(help(AUTO_NEEDS))
                                        .test_support(),
                                )
                            }),
                    ))
                    // Codex has no such list; it asks through its sandbox.
                    .when(form.runtime == Runtime::ClaudeCode, |col| {
                        col.child(field(
                            &jump,
                            "field-agent-tools",
                            "Always allow these tools",
                            Some("A comma list, passed as --allowedTools."),
                            control("agent-tools", Input::new(&form.allowed_tools)),
                        ))
                    })
                    .child(field(
                        &jump,
                        "field-agent-handoffs",
                        "Bringing in other agents",
                        Some("When this agent mentions another one."),
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(8.))
                            .child(segmented(policies))
                            .child(
                                div()
                                    .flex()
                                    .gap(ui_px(8.))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .flex_1()
                                            .gap(ui_px(4.))
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
                                            .gap(ui_px(4.))
                                            .child(help("Longest run"))
                                            .child(
                                                div()
                                                    .id("agent-max-minutes-value")
                                                    .flex()
                                                    .aria_label(limit_label)
                                                    .child(max_minutes)
                                                    .test_support(),
                                            ),
                                    ),
                            ),
                    ))
                    .child(
                        div()
                            .id("agent-credentials")
                            .mt(ui_px(16.))
                            .p(ui_px(12.))
                            .rounded(ui_px(8.))
                            .border_1()
                            .border_color(color("accent-blue"))
                            .bg(color("surface-100"))
                            .text_size(scale::text(12.))
                            .text_color(color("text-secondary"))
                            .aria_label(CREDENTIALS)
                            .child(CREDENTIALS)
                            .test_support(),
                    )
                    .when(form.editing.is_some(), |col| {
                        col.child(
                            div().mt(ui_px(24.)).child(
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

    /// The runtime's models: Claude's three, or the ones Codex lists, as
    /// a segmented control, or a menu when Codex lists more than
    /// [`MODEL_SEGMENTS`].
    fn render_model_picker(
        &self,
        form: &AgentForm,
        other_set: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let models: Vec<ModelInfo> = match form.runtime {
            Runtime::ClaudeCode => MODELS
                .iter()
                .map(|(id, label)| ModelInfo {
                    id: (*id).into(),
                    label: (*label).into(),
                })
                .collect(),
            Runtime::Codex => models_of(&self.runtimes, &Runtime::Codex).to_vec(),
        };
        let picked = match (&form.runtime, form.model.is_empty()) {
            (Runtime::Codex, true) => default_codex_model(&models),
            _ => form.model.clone(),
        };
        if models.is_empty() {
            return help("Codex lists its models once you're signed in.").into_any_element();
        }
        if models.len() <= MODEL_SEGMENTS {
            let segments = models
                .into_iter()
                .map(|m| {
                    let selected = !other_set && picked == m.id;
                    let id = m.id;
                    segment(format!("agent-model-{id}"), &m.label, selected)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_model(id.clone(), window, cx);
                        }))
                        .test_support()
                        .into_any_element()
                })
                .collect();
            return segmented(segments).into_any_element();
        }
        let label = models
            .iter()
            .find(|m| !other_set && m.id == picked)
            .map_or_else(|| "Other model".to_string(), |m| m.label.clone());
        let this = cx.entity().downgrade();
        div()
            .flex()
            .child(
                Button::new("agent-model-menu")
                    .outline()
                    .small()
                    .label(label)
                    .icon(IconName::ChevronDown)
                    .dropdown_menu(move |menu, _, _| {
                        models.iter().fold(menu, |menu, m| {
                            let this = this.clone();
                            let id = m.id.clone();
                            menu.item(PopupMenuItem::new(m.label.clone()).on_click(
                                move |_, window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.set_model(id.clone(), window, cx);
                                    })
                                    .ok();
                                },
                            ))
                        })
                    }),
            )
            .into_any_element()
    }

    /// Claude Code and Codex, then the runtimes that are coming.
    fn render_runtime_cards(&self, picked: &Runtime, cx: &mut Context<Self>) -> impl IntoElement {
        let claude = self.render_runtime_card(Runtime::ClaudeCode, picked, cx);
        let codex = self.render_runtime_card(Runtime::Codex, picked, cx);
        let coming = COMING_RUNTIMES.iter().map(|(key, name)| {
            runtime_card(SharedString::from(format!("runtime-{key}")), false, false)
                .aria_label(format!("{name} Coming soon"))
                .child(div().flex_1().child(*name))
                .child(
                    div()
                        .flex_none()
                        .text_size(scale::text(12.))
                        .child(COMING_SOON),
                )
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(COMING_SOON).build(window, cx)
                })
                .test_support()
        });
        div()
            .flex()
            .flex_col()
            .gap(ui_px(6.))
            .child(claude)
            .child(codex)
            .children(coming)
    }

    /// One runtime's card: its version and path, or how to make it ready,
    /// and its state. You can pick it once it is installed.
    fn render_runtime_card(
        &self,
        runtime: Runtime,
        picked: &Runtime,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (key, bin, name, install, login) = match runtime {
            Runtime::ClaudeCode => ("claude", "claude", "Claude Code", INSTALL_CLAUDE, None),
            Runtime::Codex => ("codex", "codex", "Codex", INSTALL_CODEX, Some(CODEX_LOGIN)),
        };
        let info = self.runtimes.iter().find(|r| r.runtime == runtime);
        let (detail, status, token) = match (self.runtimes.is_empty(), info) {
            (true, _) => (String::new(), "Checking…", "text-tertiary"),
            (false, Some(info)) if info.installed => {
                let (word, token) = runtime_state(info);
                let detail = match login.filter(|_| info.signed_in == Some(false)) {
                    Some(login) => login.to_string(),
                    None => {
                        let version = info.version.as_deref().unwrap_or("(version unknown)");
                        [Some(format!("{bin} {version}")), info.path.clone()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · ")
                    }
                };
                (detail, word, token)
            }
            (false, info) => {
                let (word, token) = info.map_or(("Not installed", "text-tertiary"), runtime_state);
                (install.to_string(), word, token)
            }
        };
        // Claude Code stays pickable while crewd checks, as before.
        let enabled = match runtime {
            Runtime::ClaudeCode => true,
            Runtime::Codex => info.is_some_and(|i| i.installed),
        };
        let selected = *picked == runtime;
        let icon = provider_icon(&runtime, 16.);
        runtime_card(
            SharedString::from(format!("runtime-{key}")),
            selected,
            enabled,
        )
        .aria_label(if selected {
            format!("{name} {status} (selected)")
        } else {
            format!("{name} {status}")
        })
        .when(enabled, |card| {
            card.cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_runtime(runtime.clone(), window, cx);
                }))
        })
        .child(icon)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().font_weight(weight(FontWeight::MEDIUM)).child(name))
                .when(!detail.is_empty(), |col| {
                    col.child(
                        div()
                            .id(SharedString::from(format!("runtime-{key}-detail")))
                            .text_size(scale::mono(12.))
                            .text_color(color("text-tertiary"))
                            .font_family(code_family())
                            .aria_label(detail.clone())
                            .child(detail.clone())
                            .test_support(),
                    )
                }),
        )
        .child(
            div()
                .flex_none()
                .text_size(scale::text(12.))
                .text_color(color(token))
                .child(status),
        )
        .test_support()
        .into_any_element()
    }
}

/// A radio card. Disabled cards are dimmed and do nothing.
fn runtime_card(id: impl Into<SharedString>, selected: bool, enabled: bool) -> Stateful<Div> {
    div()
        .id(ElementId::Name(id.into()))
        .flex()
        .items_center()
        .gap(ui_px(10.))
        .min_h(scale::row(44.))
        .px(ui_px(12.))
        .py(scale::row(8.))
        .rounded(ui_px(8.))
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
                .size(ui_px(14.))
                .rounded_full()
                .border_1()
                .border_color(color(if selected {
                    "accent-blue"
                } else {
                    "border-200"
                }))
                .when(selected, |dot| {
                    dot.flex().items_center().justify_center().child(
                        div()
                            .size(ui_px(6.))
                            .rounded_full()
                            .bg(color("accent-blue")),
                    )
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_NEEDS, ClaudeCheck, Draft, build_spec, check_name, claude_check, default_codex_model,
        runtime_usable, split_tools,
    };
    use crew_core::agent::{PermissionMode, Runtime};
    use crew_proto::{ModelInfo, RuntimeInfo};

    fn draft<'a>(name: &'a str) -> Draft<'a> {
        Draft {
            name,
            role: " Engineer ",
            runtime: Runtime::ClaudeCode,
            model: "sonnet",
            other_model: "",
            instructions: "",
            permission: PermissionMode::EditFiles,
            allowed_tools: "Bash(cargo test:*), Bash(git diff:*)",
            policy: None,
            budget: "",
            max_minutes: 0,
        }
    }

    #[test]
    fn auto_needs_a_recent_claude_model_but_not_for_codex() {
        let auto = |runtime: Runtime, model: &'static str, other: &'static str| {
            build_spec(
                &Draft {
                    runtime,
                    model,
                    other_model: other,
                    permission: PermissionMode::Auto,
                    ..draft("Forge")
                },
                &[],
            )
        };
        assert_eq!(
            auto(Runtime::ClaudeCode, "sonnet", "")
                .unwrap()
                .permission_mode,
            PermissionMode::Auto
        );
        assert_eq!(
            auto(Runtime::ClaudeCode, "sonnet", "claude-sonnet-4-5"),
            Err(AUTO_NEEDS.to_string())
        );
        assert!(auto(Runtime::ClaudeCode, "sonnet", "claude-opus-4-6").is_ok());
        assert!(auto(Runtime::Codex, "gpt-old-1", "").is_ok());
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
        assert_eq!(spec.max_run_minutes, 0, "no limit");

        let mut other = draft("Forge");
        other.other_model = " claude-opus-4-1 ";
        other.budget = "5";
        assert_eq!(build_spec(&other, &[]).unwrap().model, "claude-opus-4-1");
        assert_eq!(build_spec(&other, &[]).unwrap().handoff_budget, Some(5));

        let mut hour = draft("Forge");
        hour.max_minutes = 60;
        assert_eq!(build_spec(&hour, &[]).unwrap().max_run_minutes, 60);
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

    fn model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            label: id.into(),
        }
    }

    #[test]
    fn codex_starts_on_luna_else_the_first_model() {
        let models = [
            model("gpt-6.1-sol"),
            model("gpt-6-luna"),
            model("gpt-6-astra"),
        ];
        assert_eq!(default_codex_model(&models), "gpt-6-luna");
        assert_eq!(default_codex_model(&models[..1]), "gpt-6.1-sol");
        assert_eq!(default_codex_model(&[]), "");
    }

    #[test]
    fn a_codex_draft_keeps_its_runtime() {
        let mut d = draft("Nova");
        d.runtime = Runtime::Codex;
        d.model = "gpt-6-luna";
        let spec = build_spec(&d, &[]).unwrap();
        assert_eq!(spec.runtime, Runtime::Codex);
        assert_eq!(spec.model, "gpt-6-luna");
        assert!(
            spec.allowed_tools.is_empty(),
            "Codex takes no tool list: {:?}",
            spec.allowed_tools
        );
    }

    #[test]
    fn codex_is_usable_once_installed_and_not_signed_out() {
        let codex = |installed, signed_in| RuntimeInfo {
            runtime: Runtime::Codex,
            installed,
            version: None,
            signed_in,
            path: None,
            models: vec![],
        };
        assert!(!runtime_usable(&Runtime::Codex, &[]));
        assert!(runtime_usable(&Runtime::Codex, &[codex(true, Some(true))]));
        assert!(runtime_usable(&Runtime::Codex, &[codex(true, None)]));
        assert!(!runtime_usable(
            &Runtime::Codex,
            &[codex(true, Some(false))]
        ));
        assert!(!runtime_usable(&Runtime::Codex, &[codex(false, None)]));
        // Claude Code is judged on its own: not listed is not installed.
        assert!(!runtime_usable(
            &Runtime::ClaudeCode,
            &[codex(true, Some(true))]
        ));
    }
}
