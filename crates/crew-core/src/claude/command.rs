//! Builds the `claude` command line for an agent.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;
use uuid::Uuid;

use crate::agent::{AgentSpec, PermissionMode};
use crate::ids::RunId;

/// Name of the permission prompt tool crew-mcp serves.
pub const APPROVE_TOOL: &str = "mcp__crew__approve";

/// Tools an `EditFiles` agent may use without asking when crew-mcp is wired.
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

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
    /// The `--mcp-config` value: one stdio server named `crew`.
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
/// `mcp__crew__approve` before anything but file edits.
pub fn build_claude(
    agent: &AgentSpec,
    prompt: &str,
    cwd: &Path,
    session: SessionArg,
    mcp: Option<McpWiring>,
) -> ClaudeInvocation {
    let edits = agent.permission_mode == PermissionMode::EditFiles;
    let mode = match (&agent.permission_mode, &mcp) {
        (PermissionMode::ReadOnly, _) => "plan",
        (PermissionMode::EditFiles, None) => "acceptEdits",
        (PermissionMode::EditFiles, Some(_)) => "default",
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
    if !agent.instructions.is_empty() {
        args.push("--append-system-prompt".into());
        args.push(agent.instructions.clone());
    }
    let mut allowed = agent.allowed_tools.clone();
    if edits && mcp.is_some() {
        // The agent's own list first, then the edit tools, each once.
        let mut merged: Vec<String> = Vec::new();
        for tool in allowed.iter().map(String::as_str).chain(EDIT_TOOLS) {
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

    fn expected_mcp_config(run: RunId) -> String {
        format!(
            r#"{{"mcpServers":{{"crew":{{"type":"stdio","command":"/work/bin/crew-mcp","args":["--run","{run}"],"env":{{"CREW_HOME":"/work/crew \"home\""}}}}}}}}"#
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
                "Read,Write,Grep,Edit,MultiEdit,NotebookEdit",
                "--mcp-config",
                &config,
                "--permission-prompt-tool",
                "mcp__crew__approve",
                "--",
                "go",
            ])
        );
        let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(
            parsed["mcpServers"]["crew"]["env"]["CREW_HOME"],
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
                "--mcp-config",
                &config,
                "--",
                "go",
            ])
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
