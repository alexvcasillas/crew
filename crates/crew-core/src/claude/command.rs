//! Builds the `claude` command line for an agent.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;
use uuid::Uuid;

use crate::agent::{AgentSpec, PermissionMode};
use crate::ids::RunId;

/// The name crew-mcp is registered under for a run, for both runtimes.
/// Distinct from a plain `crew` so a server of the user's own never
/// shares its name, settings or auto-accepted asks.
pub const MCP_SERVER: &str = "crew_run";

/// Name of the permission prompt tool crew-mcp serves.
pub const APPROVE_TOOL: &str = "mcp__crew_run__approve";

/// The handoff tool crew-mcp serves. Every wired agent may use it.
pub const HANDOFF_TOOL: &str = "mcp__crew_run__handoff";

/// Edits an `EditFiles` agent may make without asking when crew-mcp is
/// wired: only under its working folder. Edits anywhere else ask.
const EDIT_TOOLS: [&str; 4] = [
    "Edit(./**)",
    "Write(./**)",
    "MultiEdit(./**)",
    "NotebookEdit(./**)",
];

/// The `(./**)` form of a bare edit tool, else `tool` as it is.
fn scoped_edit(tool: &str) -> &str {
    EDIT_TOOLS
        .iter()
        .find(|scoped| scoped.strip_suffix("(./**)") == Some(tool))
        .copied()
        .unwrap_or(tool)
}

/// The path a tool call works on, if it has one.
fn call_path(input: &serde_json::Value) -> Option<&str> {
    ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|k| input.get(*k).and_then(serde_json::Value::as_str))
        .filter(|p| !p.is_empty())
}

/// Said when a stopped call can't be allowed by a rule that matches it
/// and nothing else.
pub const NO_EXACT_RULE: &str = "This call can't be allowed exactly, so Crew won't retry it. Reply in the thread to ask for another way.";

/// Characters that would let a rule match more than one call, or that
/// Claude Code reads as part of the rule's own syntax.
const NOT_EXACT: [char; 12] = [
    '*', ',', '(', ')', '\\', '{', '}', '?', '[', ']', '\n', '\r',
];

/// Tools whose rule names a single file.
const PATH_TOOLS: [&str; 5] = ["Read", "Edit", "Write", "MultiEdit", "NotebookEdit"];

/// An allow rule for exactly one call, or `None` if there is no such
/// rule: `Bash(<command>)` for a command, else a file tool with the file
/// it works on. An absolute path is written `//path`, as Claude Code reads
/// a single `/` as relative to the settings; a relative one `./path`, so
/// it never matches the same name in another folder. MCP tools, web tools
/// and anything else get no rule, as does any text with a wildcard, a
/// separator or a line break in it.
pub fn retry_rule(tool: &str, input: &serde_json::Value) -> Option<String> {
    let exact = |text: &str| !text.trim().is_empty() && !text.contains(NOT_EXACT);
    if tool == "Bash" {
        let command = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .filter(|c| exact(c))?;
        return Some(format!("Bash({command})"));
    }
    if !PATH_TOOLS.contains(&tool) {
        return None;
    }
    let path = call_path(input).filter(|p| exact(p))?;
    if path.starts_with(['~', '!', '#']) || path.split('/').any(|part| part == "..") {
        return None;
    }
    Some(if path.starts_with("//") {
        format!("{tool}({path})")
    } else if path.starts_with('/') {
        format!("{tool}(/{path})")
    } else if path.starts_with("./") {
        format!("{tool}({path})")
    } else {
        format!("{tool}(./{path})")
    })
}

/// The `--settings` value that allows `rules` for one run.
fn allow_settings(rules: &[String]) -> String {
    serde_json::json!({ "permissions": { "allow": rules } }).to_string()
}

/// The prompt of the run that retries a call the safety check stopped.
pub fn retry_prompt(tool: &str, input: &serde_json::Value) -> String {
    let what = match input.get("command").and_then(serde_json::Value::as_str) {
        Some(command) if tool == "Bash" => command.to_string(),
        _ => match call_path(input) {
            Some(path) => format!("{tool} {path}"),
            None => tool.to_string(),
        },
    };
    format!(
        "The safety check stopped `{what}`. The user allowed it once. Run it now if you still need it, then carry on."
    )
}

/// What a run needs to reach crewd through crew-mcp.
#[derive(Debug, Clone, PartialEq)]
pub struct McpWiring {
    pub crew_mcp_bin: PathBuf,
    pub run: RunId,
    pub crew_home: PathBuf,
}

#[derive(Serialize)]
struct McpConfig<'a> {
    #[serde(rename = "mcpServers")]
    mcp_servers: McpServers<'a>,
}

#[derive(Serialize)]
struct McpServers<'a> {
    /// Keep in step with [`MCP_SERVER`].
    #[serde(rename = "crew_run")]
    crew: StdioServer<'a>,
}

#[derive(Serialize)]
struct StdioServer<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    command: String,
    args: [String; 2],
    env: CrewEnv,
}

#[derive(Serialize)]
struct CrewEnv {
    #[serde(rename = "CREW_HOME")]
    crew_home: String,
}

impl McpWiring {
    /// The `--mcp-config` value: one stdio server named [`MCP_SERVER`].
    pub fn config_json(&self) -> String {
        let config = McpConfig {
            mcp_servers: McpServers {
                crew: StdioServer {
                    kind: "stdio",
                    command: self.crew_mcp_bin.to_string_lossy().into_owned(),
                    args: ["--run".into(), self.run.to_string()],
                    env: CrewEnv {
                        crew_home: self.crew_home.to_string_lossy().into_owned(),
                    },
                },
            },
        };
        serde_json::to_string(&config).expect("the MCP config always serializes")
    }
}

/// Another agent the running agent can bring in.
#[derive(Debug, Clone, PartialEq)]
pub struct Teammate {
    pub name: String,
    pub role: String,
    pub edits: bool,
}

/// Where the agent works and with whom. Told to it in the system prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct CrewContext {
    pub project: String,
    /// Live agents other than the one running.
    pub team: Vec<Teammate>,
}

impl CrewContext {
    /// The plain text block for `agent`. `wired` adds how to hand off.
    pub fn render(&self, agent: &AgentSpec, wired: bool) -> String {
        let handoff = wired.then(|| {
            format!(
                "To bring in another agent, call the {HANDOFF_TOOL} tool with the agent's name and a short brief. Writing @Name in your final reply also works."
            )
        });
        self.render_with(agent, handoff)
    }

    /// [`CrewContext::render`] for a Codex agent, which sees crew-mcp's
    /// tools by server and tool name.
    pub fn render_codex(&self, agent: &AgentSpec, wired: bool) -> String {
        let handoff = wired.then(|| {
            "To bring in another agent, call the handoff tool of the crew_run MCP server with the agent's name and a short brief. Writing @Name in your final reply also works.".to_string()
        });
        self.render_with(agent, handoff)
    }

    fn render_with(&self, agent: &AgentSpec, handoff: Option<String>) -> String {
        let who = if agent.role.trim().is_empty() {
            agent.name.clone()
        } else {
            format!("{}, {}", agent.name, agent.role.trim())
        };
        let mut lines = vec![format!(
            "You are {who}, working in Crew with other agents in project {}.",
            self.project
        )];
        lines.push(match agent.permission_mode {
            PermissionMode::EditFiles => "You can edit files in this thread's worktree.".into(),
            PermissionMode::Auto => "You can edit files in this thread's worktree. Commands run without asking unless a safety check stops them.".into(),
            PermissionMode::ReadOnly => {
                "You are read-only: review and report, don't change files.".into()
            }
        });
        if !self.team.is_empty() {
            lines.push("Other agents here:".into());
            for mate in &self.team {
                let access = if mate.edits {
                    "edits files"
                } else {
                    "read-only"
                };
                lines.push(if mate.role.trim().is_empty() {
                    format!("- {} ({access})", mate.name)
                } else {
                    format!("- {} ({}, {access})", mate.name, mate.role.trim())
                });
            }
        }
        lines.extend(handoff);
        lines.join("\n")
    }
}

/// How the run attaches to a Claude session.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionArg {
    /// Start a session with an id chosen by Crew.
    New(Uuid),
    /// Continue an existing session.
    Resume(String),
}

/// The program, arguments and working directory to spawn.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeInvocation {
    pub bin: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Extra environment for the process.
    pub env: Vec<(String, String)>,
}

/// Picks the binary: the given override if set, else `claude` on `PATH`.
pub fn claude_bin_from(env: Option<OsString>) -> PathBuf {
    match env {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from("claude"),
    }
}

/// Builds the command line. The prompt goes last, after `--`.
///
/// With `mcp`, the run gets crew-mcp as an MCP server. An `EditFiles`
/// agent then runs in `default` mode and asks crewd through
/// `mcp__crew_run__approve` before anything but file edits under its working
/// folder. Every wired agent may use `mcp__crew_run__handoff` without asking.
///
/// An `Auto` agent runs in `auto` mode, wired the same way: a safety
/// check lets it run what is safe, and anything else still asks.
///
/// With `context`, the Crew block goes before the agent's own instructions
/// in one `--append-system-prompt`.
///
/// `extra_allowed` are allow rules for this run only, such as the one
/// call you let a retry make. They go in `--settings` as JSON, so a rule
/// is never split on a comma the way `--allowedTools` splits its list.
pub fn build_claude(
    agent: &AgentSpec,
    prompt: &str,
    cwd: &Path,
    session: SessionArg,
    mcp: Option<McpWiring>,
    context: Option<&CrewContext>,
    extra_allowed: &[String],
) -> ClaudeInvocation {
    let edits = agent.permission_mode.edits();
    let mode = match (&agent.permission_mode, &mcp) {
        (PermissionMode::ReadOnly, _) => "plan",
        (PermissionMode::EditFiles, None) => "acceptEdits",
        (PermissionMode::EditFiles, Some(_)) => "default",
        (PermissionMode::Auto, _) => "auto",
    };
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        &agent.model,
        "--permission-mode",
        mode,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if mcp.is_none() {
        args.push("--permission-prompts".into());
        args.push("none".into());
    }
    match session {
        SessionArg::New(id) => {
            args.push("--session-id".into());
            args.push(id.to_string());
        }
        SessionArg::Resume(id) => {
            args.push("--resume".into());
            args.push(id);
        }
    }
    let system = [
        context.map(|c| c.render(agent, mcp.is_some())),
        Some(agent.instructions.clone()),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    if !system.is_empty() {
        args.push("--append-system-prompt".into());
        args.push(system);
    }
    let mut allowed = agent.allowed_tools.clone();
    if mcp.is_some() {
        // The agent's own list first, then what wiring adds, each once.
        // A bare edit rule reaches outside the working folder, so it is
        // scoped like the ones wiring adds.
        let extra: &[&str] = if edits { &EDIT_TOOLS } else { &[] };
        let mut merged: Vec<String> = Vec::new();
        for tool in allowed
            .iter()
            .map(|t| scoped_edit(t))
            .chain(extra.iter().copied())
            .chain([HANDOFF_TOOL])
        {
            if !merged.iter().any(|t| t == tool) {
                merged.push(tool.to_string());
            }
        }
        allowed = merged;
    }
    if !allowed.is_empty() {
        args.push("--allowedTools".into());
        args.push(allowed.join(","));
    }
    let mut extra: Vec<String> = Vec::new();
    for rule in extra_allowed {
        if !allowed.contains(rule) && !extra.contains(rule) {
            extra.push(rule.clone());
        }
    }
    if !extra.is_empty() {
        args.push("--settings".into());
        args.push(allow_settings(&extra));
    }
    if let Some(mcp) = &mcp {
        args.push("--mcp-config".into());
        args.push(mcp.config_json());
        if edits {
            args.push("--permission-prompt-tool".into());
            args.push(APPROVE_TOOL.into());
        }
    }
    // Last, after `--`, so a prompt that starts with `-` is never a flag.
    args.push("--".into());
    args.push(prompt.to_string());
    ClaudeInvocation {
        bin: claude_bin_from(std::env::var_os("CREW_CLAUDE_BIN")),
        args,
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Runtime;

    fn agent(mode: PermissionMode) -> AgentSpec {
        AgentSpec {
            name: "reviewer".into(),
            role: "Reviews code".into(),
            runtime: Runtime::ClaudeCode,
            model: "claude-haiku-4-5".into(),
            instructions: "Be brief.".into(),
            permission_mode: mode,
            allowed_tools: vec!["Read".into(), "Grep".into()],
            handoff_policy: None,
            handoff_budget: None,
            max_run_minutes: 20,
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn read_only_new_session_args_in_order() {
        let id = Uuid::new_v4();
        let inv = build_claude(
            &agent(PermissionMode::ReadOnly),
            "- hi",
            Path::new("/work/demo"),
            SessionArg::New(id),
            None,
            None,
            &[],
        );
        let id = id.to_string();
        assert_eq!(
            inv.args,
            args(&[
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-haiku-4-5",
                "--permission-mode",
                "plan",
                "--permission-prompts",
                "none",
                "--session-id",
                &id,
                "--append-system-prompt",
                "Be brief.",
                "--allowedTools",
                "Read,Grep",
                "--",
                "- hi",
            ])
        );
        assert_eq!(inv.cwd, Path::new("/work/demo"));
    }

    #[test]
    fn edit_agent_resumes_with_bare_args_in_order() {
        let mut a = agent(PermissionMode::EditFiles);
        a.instructions.clear();
        a.allowed_tools.clear();
        let inv = build_claude(
            &a,
            "--help",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            None,
            None,
            &[],
        );
        assert_eq!(
            inv.args,
            args(&[
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-haiku-4-5",
                "--permission-mode",
                "acceptEdits",
                "--permission-prompts",
                "none",
                "--resume",
                "abc",
                "--",
                "--help",
            ])
        );
    }

    fn wiring(run: RunId) -> McpWiring {
        McpWiring {
            crew_mcp_bin: PathBuf::from("/work/bin/crew-mcp"),
            run,
            crew_home: PathBuf::from("/work/crew \"home\""),
        }
    }

    #[test]
    fn tool_names_use_the_server_name() {
        assert_eq!(APPROVE_TOOL, format!("mcp__{MCP_SERVER}__approve"));
        assert_eq!(HANDOFF_TOOL, format!("mcp__{MCP_SERVER}__handoff"));
    }

    fn expected_mcp_config(run: RunId) -> String {
        format!(
            r#"{{"mcpServers":{{"crew_run":{{"type":"stdio","command":"/work/bin/crew-mcp","args":["--run","{run}"],"env":{{"CREW_HOME":"/work/crew \"home\""}}}}}}}}"#
        )
    }

    #[test]
    fn edit_agent_with_wiring_asks_through_crew() {
        let run = RunId::new();
        let mut a = agent(PermissionMode::EditFiles);
        a.allowed_tools = vec!["Read".into(), "Write".into(), "Grep".into(), "Read".into()];
        let inv = build_claude(
            &a,
            "go",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            Some(wiring(run)),
            None,
            &[],
        );
        let config = expected_mcp_config(run);
        assert_eq!(
            inv.args,
            args(&[
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-haiku-4-5",
                "--permission-mode",
                "default",
                "--resume",
                "abc",
                "--append-system-prompt",
                "Be brief.",
                "--allowedTools",
                "Read,Write(./**),Grep,Edit(./**),MultiEdit(./**),NotebookEdit(./**),mcp__crew_run__handoff",
                "--mcp-config",
                &config,
                "--permission-prompt-tool",
                "mcp__crew_run__approve",
                "--",
                "go",
            ])
        );
        let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(
            parsed["mcpServers"]["crew_run"]["env"]["CREW_HOME"],
            "/work/crew \"home\""
        );
    }

    #[test]
    fn read_only_agent_with_wiring_gets_handoff_but_no_prompt_tool() {
        let run = RunId::new();
        let mut a = agent(PermissionMode::ReadOnly);
        a.allowed_tools.clear();
        let inv = build_claude(
            &a,
            "go",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            Some(wiring(run)),
            None,
            &[],
        );
        let config = expected_mcp_config(run);
        assert_eq!(
            inv.args,
            args(&[
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-haiku-4-5",
                "--permission-mode",
                "plan",
                "--resume",
                "abc",
                "--append-system-prompt",
                "Be brief.",
                "--allowedTools",
                "mcp__crew_run__handoff",
                "--mcp-config",
                &config,
                "--",
                "go",
            ])
        );
    }

    #[test]
    fn wiring_scopes_an_agents_own_edit_rules() {
        let mut a = agent(PermissionMode::ReadOnly);
        a.allowed_tools = [
            "Read",
            "Edit",
            "Write(src/**)",
            "MultiEdit",
            "Edit(./**)",
            "NotebookEdit",
        ]
        .map(String::from)
        .to_vec();
        let inv = build_claude(
            &a,
            "go",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            Some(wiring(RunId::new())),
            None,
            &[],
        );
        let allowed = inv
            .args
            .iter()
            .skip_while(|a| *a != "--allowedTools")
            .nth(1)
            .unwrap();
        assert_eq!(
            allowed,
            "Read,Edit(./**),Write(src/**),MultiEdit(./**),NotebookEdit(./**),mcp__crew_run__handoff"
        );

        // Without wiring the agent's list is passed as it is.
        let inv = build_claude(
            &a,
            "go",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            None,
            None,
            &[],
        );
        assert!(
            inv.args
                .contains(&"Read,Edit,Write(src/**),MultiEdit,Edit(./**),NotebookEdit".to_string())
        );
    }

    fn context() -> CrewContext {
        CrewContext {
            project: "demo".into(),
            team: vec![
                Teammate {
                    name: "Forge".into(),
                    role: "Builds features".into(),
                    edits: true,
                },
                Teammate {
                    name: "Scout".into(),
                    role: String::new(),
                    edits: false,
                },
            ],
        }
    }

    fn system_prompt(inv: &ClaudeInvocation) -> Vec<&String> {
        inv.args
            .windows(2)
            .filter(|w| w[0] == "--append-system-prompt")
            .map(|w| &w[1])
            .collect()
    }

    #[test]
    fn crew_context_joins_instructions_in_one_system_prompt() {
        let inv = build_claude(
            &agent(PermissionMode::ReadOnly),
            "go",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            Some(wiring(RunId::new())),
            Some(&context()),
            &[],
        );
        assert_eq!(
            system_prompt(&inv),
            vec![
                "You are reviewer, Reviews code, working in Crew with other agents in project demo.\n\
                 You are read-only: review and report, don't change files.\n\
                 Other agents here:\n\
                 - Forge (Builds features, edits files)\n\
                 - Scout (read-only)\n\
                 To bring in another agent, call the mcp__crew_run__handoff tool with the agent's name and a short brief. Writing @Name in your final reply also works.\n\
                 \n\
                 Be brief."
            ]
        );
    }

    #[test]
    fn crew_context_without_wiring_or_instructions() {
        let mut a = agent(PermissionMode::EditFiles);
        a.instructions.clear();
        let ctx = CrewContext {
            project: "demo".into(),
            team: vec![],
        };
        let inv = build_claude(
            &a,
            "go",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            None,
            Some(&ctx),
            &[],
        );
        assert_eq!(
            system_prompt(&inv),
            vec![
                "You are reviewer, Reviews code, working in Crew with other agents in project demo.\n\
                 You can edit files in this thread's worktree."
            ]
        );
    }

    #[test]
    fn codex_context_names_the_handoff_tool_of_the_crew_server() {
        let a = agent(PermissionMode::EditFiles);
        assert_eq!(
            context().render_codex(&a, true),
            "You are reviewer, Reviews code, working in Crew with other agents in project demo.\n\
             You can edit files in this thread's worktree.\n\
             Other agents here:\n\
             - Forge (Builds features, edits files)\n\
             - Scout (read-only)\n\
             To bring in another agent, call the handoff tool of the crew_run MCP server with the agent's name and a short brief. Writing @Name in your final reply also works."
        );
        let plain = context().render_codex(&a, false);
        assert!(!plain.contains("handoff"));
        assert_eq!(plain, context().render(&a, false));
    }

    #[test]
    fn auto_agent_runs_in_auto_mode_wired_like_an_edit_agent() {
        let run = RunId::new();
        let mut a = agent(PermissionMode::Auto);
        a.instructions.clear();
        a.allowed_tools = vec!["Read".into(), "Bash(npm test)".into()];
        let extra = vec!["Bash(rm -rf build)".to_string(), "Read".to_string()];
        let inv = build_claude(
            &a,
            "go",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            Some(wiring(run)),
            None,
            &extra,
        );
        let config = expected_mcp_config(run);
        assert_eq!(
            inv.args,
            args(&[
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-haiku-4-5",
                "--permission-mode",
                "auto",
                "--resume",
                "abc",
                "--allowedTools",
                "Read,Bash(npm test),Edit(./**),Write(./**),MultiEdit(./**),NotebookEdit(./**),mcp__crew_run__handoff",
                "--settings",
                r#"{"permissions":{"allow":["Bash(rm -rf build)"]}}"#,
                "--mcp-config",
                &config,
                "--permission-prompt-tool",
                "mcp__crew_run__approve",
                "--",
                "go",
            ])
        );
    }

    #[test]
    fn auto_agent_without_wiring_turns_prompts_off() {
        let mut a = agent(PermissionMode::Auto);
        a.instructions.clear();
        a.allowed_tools.clear();
        let extra = vec!["Bash(ls)".to_string(), "Bash(ls)".to_string()];
        let inv = build_claude(
            &a,
            "go",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
            None,
            None,
            &extra,
        );
        assert_eq!(
            inv.args,
            args(&[
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-haiku-4-5",
                "--permission-mode",
                "auto",
                "--permission-prompts",
                "none",
                "--resume",
                "abc",
                "--settings",
                r#"{"permissions":{"allow":["Bash(ls)"]}}"#,
                "--",
                "go",
            ])
        );
    }

    #[test]
    fn auto_context_says_commands_run_unless_stopped() {
        let ctx = CrewContext {
            project: "demo".into(),
            team: vec![],
        };
        assert_eq!(
            ctx.render(&agent(PermissionMode::Auto), false),
            "You are reviewer, Reviews code, working in Crew with other agents in project demo.\n\
             You can edit files in this thread's worktree. Commands run without asking unless a safety check stops them."
        );
    }

    #[test]
    fn a_retry_allows_exactly_the_stopped_call() {
        use serde_json::json;
        let bash = |command: &str| retry_rule("Bash", &json!({ "command": command }));
        assert_eq!(bash("rm -rf build").as_deref(), Some("Bash(rm -rf build)"));
        assert_eq!(
            retry_rule("Write", &json!({"file_path": "/work/demo/a.txt"})).as_deref(),
            Some("Write(//work/demo/a.txt)")
        );
        assert_eq!(
            retry_rule("Edit", &json!({"file_path": "src/a.rs"})).as_deref(),
            Some("Edit(./src/a.rs)")
        );
        assert_eq!(
            retry_rule("Read", &json!({"file_path": "a.rs"})).as_deref(),
            Some("Read(./a.rs)"),
            "never any a.rs in another folder"
        );
        // Anything that could match more, or split, is refused.
        for command in [
            "rm -rf *",
            "git log --format=%h,%s",
            "echo (x)",
            "echo a)",
            "rm a\nrm b",
            "ls ?",
            "ls [ab]",
            "echo {a,b}",
            "echo a\\b",
            "   ",
        ] {
            assert_eq!(bash(command), None, "{command:?}");
        }
        assert_eq!(retry_rule("Bash", &json!({})), None, "no command");
        assert_eq!(
            retry_rule("mcp__github__create_issue", &json!({"title": "x"})),
            None
        );
        assert_eq!(retry_rule("WebFetch", &json!({"url": "https://x"})), None);
        assert_eq!(retry_rule("WebSearch", &json!({"query": "x"})), None);
        assert_eq!(retry_rule("Glob", &json!({"path": "src"})), None);
        assert_eq!(retry_rule("Write", &json!({})), None, "no path");
        for path in ["src/**/a.rs", "../a.rs", "~/a.rs", "a\nb"] {
            assert_eq!(
                retry_rule("Write", &json!({ "file_path": path })),
                None,
                "{path:?}"
            );
        }
        assert_eq!(
            retry_prompt("Bash", &json!({"command": "rm -rf build"})),
            "The safety check stopped `rm -rf build`. The user allowed it once. Run it now if you still need it, then carry on."
        );
        assert_eq!(
            retry_prompt("Write", &json!({"file_path": "/work/demo/a.txt"})),
            "The safety check stopped `Write /work/demo/a.txt`. The user allowed it once. Run it now if you still need it, then carry on."
        );
    }

    #[test]
    fn bin_honours_crew_claude_bin_env() {
        assert_eq!(claude_bin_from(None), PathBuf::from("claude"));
        assert_eq!(
            claude_bin_from(Some("/work/fake-claude".into())),
            PathBuf::from("/work/fake-claude")
        );
    }
}
