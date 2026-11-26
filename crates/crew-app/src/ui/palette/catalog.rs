//! What the palette can search besides places: the commands behind the
//! shortcuts list, and every field of every settings page. Both are plain
//! data; matching and jumping live with the rows that show them.

use crew_core::agent::PermissionMode;
use crew_core::ids::{AgentId, ProjectId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::TitleMode;
use gpui_kit::{Action, App, OperatingSystem};

use crate::app::{
    AddProject, ApproveLatest, ArchiveThread, RenameThread, StopAll, bindings as app_bindings,
};
use crate::prefs::{Density, LineHeight, SidebarMode};
use crate::theme::scale;
use crate::ui::settings::agents::MODELS;
use crate::ui::settings::runtimes::{runtime_name, runtime_state};
use crate::ui::settings::{Page, pretty_keys};
use crate::ui::workspace::Workspace;

/// Whose settings a field belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    /// One field per agent, so a match names the agent too.
    Agent,
    /// One field per project.
    Project,
}

/// One settings field: where it lives and what finds it.
#[derive(Debug, Clone, Copy)]
pub struct SettingEntry {
    pub page: Page,
    pub scope: Scope,
    /// The element id of the field's row, which starts with `field-`.
    pub field: &'static str,
    pub label: &'static str,
    /// Other words people use for it.
    pub keywords: &'static [&'static str],
    /// The names of the choices the field offers, as the page shows them.
    pub options: &'static [&'static str],
}

/// The agent or project an entry of a scoped page is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Agent(AgentId),
    Project(ProjectId),
}

const fn entry(
    page: Page,
    scope: Scope,
    field: &'static str,
    label: &'static str,
    keywords: &'static [&'static str],
    options: &'static [&'static str],
) -> SettingEntry {
    SettingEntry {
        page,
        scope,
        field,
        label,
        keywords,
        options,
    }
}

const SIZES_UI: [&str; 6] = ["12", "13", "14", "15", "16", "18"];
const SIZES_CODE: [&str; 6] = ["11", "12", "13", "14", "15", "16"];
const AT_ONCE: [&str; 8] = ["1", "2", "3", "4", "5", "6", "7", "8"];
const BUDGETS: [&str; 5] = ["1", "2", "3", "5", "10"];

/// Every field of every settings page, in the order the pages show them.
static SETTINGS: &[SettingEntry] = &[
    entry(
        Page::General,
        Scope::Global,
        "field-general-thread-titles",
        "Thread titles",
        &[
            "title",
            "name",
            "naming",
            "model",
            "haiku",
            "sonnet",
            "codex",
            "cheapest",
            "comparison",
        ],
        &[
            "Generated",
            "First line of the message",
            "Haiku",
            "Sonnet",
            "Codex, cheapest model",
        ],
    ),
    entry(
        Page::General,
        Scope::Global,
        "field-general-home",
        "Crew home",
        &["folder", "directory", "database", "socket", "worktrees"],
        &[],
    ),
    entry(
        Page::General,
        Scope::Global,
        "field-general-editor",
        "Editor",
        &["open in editor", "app", "vscode", "zed", "cursor", "ide"],
        &["Finder"],
    ),
    entry(
        Page::Appearance,
        Scope::Global,
        "field-appearance-ui-size",
        "Interface text size",
        &["font size", "text", "bigger", "smaller", "larger", "size"],
        &SIZES_UI,
    ),
    entry(
        Page::Appearance,
        Scope::Global,
        "field-appearance-code-size",
        "Code text size",
        &[
            "font size",
            "monospace",
            "terminal",
            "diff",
            "bigger",
            "smaller",
            "size",
        ],
        &SIZES_CODE,
    ),
    entry(
        Page::Appearance,
        Scope::Global,
        "field-appearance-fonts",
        "Fonts",
        &["typeface", "font family", "interface", "code", "mono"],
        &["Geist", "SF Pro", "Geist Mono", "SF Mono", "Menlo"],
    ),
    entry(
        Page::Appearance,
        Scope::Global,
        "field-appearance-line-height",
        "Line height",
        &["spacing", "leading", "lines"],
        &["Compact", "Comfortable", "Roomy"],
    ),
    entry(
        Page::Appearance,
        Scope::Global,
        "field-appearance-rendering",
        "Text rendering",
        &["bold", "weight", "heavier", "smoothing", "contrast"],
        &["Standard", "Bolder text"],
    ),
    entry(
        Page::Appearance,
        Scope::Global,
        "field-appearance-density",
        "Density",
        &["padding", "spacing", "rows", "tight", "compact"],
        &["Compact", "Comfortable"],
    ),
    entry(
        Page::Appearance,
        Scope::Global,
        "field-appearance-sidebar",
        "Sidebar",
        &["projects", "threads", "list", "layout"],
        &["Projects", "Threads"],
    ),
    entry(
        Page::Appearance,
        Scope::Global,
        "field-appearance-zoom",
        "Zoom",
        &["size", "bigger", "smaller", "scale", "magnify", "percent"],
        &[],
    ),
    entry(
        Page::Skills,
        Scope::Global,
        "field-skills-project",
        "Project skills from",
        &["skill", "commands", "slash", "project"],
        &[],
    ),
    entry(
        Page::Runtimes,
        Scope::Global,
        "field-runtimes-claude",
        "Claude Code",
        &["runtime", "cli", "claude", "sign in", "login", "installed"],
        &[],
    ),
    entry(
        Page::Runtimes,
        Scope::Global,
        "field-runtimes-codex",
        "Codex",
        &["runtime", "cli", "openai", "sign in", "login", "installed"],
        &[],
    ),
    entry(
        Page::Limits,
        Scope::Global,
        "field-limits-max-runs",
        "Agents at once",
        &["concurrent", "parallel", "runs", "queue", "limit"],
        &AT_ONCE,
    ),
    entry(
        Page::Limits,
        Scope::Global,
        "field-limits-new-runs",
        "New runs",
        &["pause", "resume", "queue", "hold", "stop"],
        &["Running", "Paused"],
    ),
    entry(
        Page::Limits,
        Scope::Global,
        "field-limits-policy",
        "Bringing in other agents",
        &["handoff", "handoffs", "mention", "ask", "budget", "approve"],
        &["Ask first", "Run with budget"],
    ),
    entry(
        Page::Limits,
        Scope::Global,
        "field-limits-budget",
        "Handoffs per thread",
        &["handoff", "handoffs", "budget", "limit", "mentions"],
        &BUDGETS,
    ),
    entry(
        Page::Agents,
        Scope::Agent,
        "field-agent-name",
        "Name and role",
        &["mention", "rename", "title", "job"],
        &[],
    ),
    entry(
        Page::Agents,
        Scope::Agent,
        "field-agent-runtime",
        "Runtime",
        &["cli", "claude", "codex", "provider"],
        &["Claude Code", "Codex"],
    ),
    entry(
        Page::Agents,
        Scope::Agent,
        "field-agent-model",
        "Model",
        &["opus", "sonnet", "haiku", "gpt", "--model"],
        &["Opus", "Sonnet", "Haiku"],
    ),
    entry(
        Page::Agents,
        Scope::Agent,
        "field-agent-instructions",
        "Instructions",
        &["prompt", "system", "claude.md", "agents.md", "rules"],
        &[],
    ),
    entry(
        Page::Agents,
        Scope::Agent,
        "field-agent-permission",
        "What it can do",
        &[
            "permission",
            "permissions",
            "mode",
            "plan",
            "auto",
            "edit",
            "read only",
            "sandbox",
        ],
        &["Edit files", "Auto", "Read only"],
    ),
    entry(
        Page::Agents,
        Scope::Agent,
        "field-agent-tools",
        "Always allow these tools",
        &["allowed tools", "allowedtools", "bash", "permission"],
        &[],
    ),
    entry(
        Page::Agents,
        Scope::Agent,
        "field-agent-handoffs",
        "Bringing in other agents",
        &[
            "handoff",
            "handoffs",
            "budget",
            "longest run",
            "timeout",
            "minutes",
            "limit",
        ],
        &["Channel default", "Ask first", "Run with budget"],
    ),
    entry(
        Page::Projects,
        Scope::Project,
        "field-project-channels",
        "Channels",
        &["handoff", "handoffs", "policy", "budget"],
        &["Ask first", "Run with budget", "Per agent"],
    ),
    entry(
        Page::Projects,
        Scope::Project,
        "field-project-setup",
        "Setup command",
        &["install", "bootstrap", "worktree", "script", "init"],
        &[],
    ),
    entry(
        Page::Projects,
        Scope::Project,
        "field-project-files",
        "Files to copy",
        &["env", "dotenv", "secrets", "worktree", "copy", "config"],
        &[],
    ),
];

/// Every field of every settings page.
pub fn settings_entries() -> &'static [SettingEntry] {
    SETTINGS
}

/// What a command needs before it can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    Nothing,
    /// A thread is open.
    OpenThread,
    /// The open thread waits on a request.
    PendingRequest,
    /// Some run is live.
    ActiveRun,
}

/// One command: what it is called, its keys and what it runs.
pub struct CommandEntry {
    pub label: &'static str,
    /// The keys as the shortcuts page shows them, if it has any.
    pub keys: Option<String>,
    pub action: Box<dyn Action>,
    pub needs: Needs,
}

fn needs(action: &dyn Action) -> Needs {
    if action.partial_eq(&RenameThread) || action.partial_eq(&ArchiveThread) {
        Needs::OpenThread
    } else if action.partial_eq(&ApproveLatest) {
        Needs::PendingRequest
    } else if action.partial_eq(&StopAll) {
        Needs::ActiveRun
    } else {
        Needs::Nothing
    }
}

/// The shortcuts list as commands, plus Add project…. An action with two
/// shortcuts shows once, with the later one, which for Running now is
/// ⌘⇧R.
pub fn commands() -> Vec<CommandEntry> {
    let mut commands: Vec<CommandEntry> = Vec::new();
    for shortcut in app_bindings() {
        let keys = shortcut
            .binding
            .keystrokes()
            .iter()
            .map(|k| k.unparse(OperatingSystem::Mac))
            .collect::<Vec<_>>()
            .join(" ");
        let action = shortcut.binding.action().boxed_clone();
        let command = CommandEntry {
            label: shortcut.label,
            keys: Some(pretty_keys(&keys)),
            needs: needs(action.as_ref()),
            action,
        };
        match commands
            .iter_mut()
            .find(|c| c.action.partial_eq(command.action.as_ref()))
        {
            Some(same) => *same = command,
            None => commands.push(command),
        }
    }
    commands.push(CommandEntry {
        label: "Add project…",
        keys: None,
        action: Box::new(AddProject),
        needs: Needs::Nothing,
    });
    commands
}

/// The value `entry` shows now, for fields with a few named choices, a
/// toggle or a select. `None` for free text and for what the page can't
/// say yet. `item` is the agent or project of a scoped entry.
pub fn current_value(
    ws: &Workspace,
    entry: &SettingEntry,
    item: Option<Item>,
    cx: &App,
) -> Option<String> {
    let look = scale::appearance(cx);
    let limits = ws.state().limits();
    let agent = match item {
        Some(Item::Agent(id)) => ws.state().agent(id).map(|a| &a.spec),
        _ => None,
    };
    let policy = |p: HandoffPolicy| match p {
        HandoffPolicy::AskFirst => "Ask first",
        HandoffPolicy::Budget => "Run with budget",
        HandoffPolicy::PerAgent => "Per agent",
    };
    let runtime = |name: &str| {
        ws.runtimes
            .iter()
            .find(|r| runtime_name(&r.runtime) == name)
            .map(|r| runtime_state(r).0.to_string())
    };
    match entry.field {
        "field-general-thread-titles" => Some(
            match ws.state().titles().mode {
                TitleMode::Generated => "Generated",
                TitleMode::FirstLine => "First line of the message",
            }
            .into(),
        ),
        "field-general-editor" => Some(ws.editor_app().unwrap_or_else(|| "Finder".into())),
        "field-appearance-ui-size" => Some(look.ui_size.to_string()),
        "field-appearance-code-size" => Some(look.code_size.to_string()),
        "field-appearance-line-height" => Some(
            match look.line_height {
                LineHeight::Compact => "Compact",
                LineHeight::Comfortable => "Comfortable",
                LineHeight::Roomy => "Roomy",
            }
            .into(),
        ),
        "field-appearance-rendering" => Some(
            if look.bolder {
                "Bolder text"
            } else {
                "Standard"
            }
            .into(),
        ),
        "field-appearance-density" => Some(
            match look.density {
                Density::Compact => "Compact",
                Density::Comfortable => "Comfortable",
            }
            .into(),
        ),
        "field-appearance-sidebar" => Some(
            match look.sidebar_mode {
                SidebarMode::Projects => "Projects",
                SidebarMode::Threads => "Threads",
            }
            .into(),
        ),
        "field-appearance-zoom" => Some(format!("{}%", look.zoom)),
        "field-runtimes-claude" => runtime("Claude Code"),
        "field-runtimes-codex" => runtime("Codex"),
        "field-limits-max-runs" => Some(limits.max_runs.to_string()),
        "field-limits-new-runs" => Some(if limits.paused { "Paused" } else { "Running" }.into()),
        "field-limits-policy" => Some(policy(limits.handoff_policy).into()),
        "field-limits-budget" => Some(limits.handoff_budget.to_string()),
        "field-agent-runtime" => agent.map(|a| runtime_name(&a.runtime).to_string()),
        "field-agent-model" => agent.filter(|a| !a.model.is_empty()).map(|a| {
            MODELS
                .iter()
                .find(|(id, _)| *id == a.model)
                .map_or_else(|| a.model.clone(), |(_, label)| (*label).to_string())
        }),
        "field-agent-permission" => agent.map(|a| {
            match a.permission_mode {
                PermissionMode::EditFiles => "Edit files",
                PermissionMode::Auto => "Auto",
                PermissionMode::ReadOnly => "Read only",
            }
            .to_string()
        }),
        "field-agent-handoffs" => agent.map(|a| {
            a.handoff_policy
                .map_or("Channel default", policy)
                .to_string()
        }),
        _ => None,
    }
}
