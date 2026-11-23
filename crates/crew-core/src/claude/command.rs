//! Builds the `claude` command line for an agent.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::agent::{AgentSpec, PermissionMode};

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
pub fn build_claude(
    agent: &AgentSpec,
    prompt: &str,
    cwd: &Path,
    session: SessionArg,
) -> ClaudeInvocation {
    let mode = match agent.permission_mode {
        PermissionMode::ReadOnly => "plan",
        PermissionMode::EditFiles => "acceptEdits",
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
        "--permission-prompts",
        "none",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
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
    if !agent.allowed_tools.is_empty() {
        args.push("--allowedTools".into());
        args.push(agent.allowed_tools.join(","));
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

    #[test]
    fn bin_honours_crew_claude_bin_env() {
        assert_eq!(claude_bin_from(None), PathBuf::from("claude"));
        assert_eq!(
            claude_bin_from(Some("/work/fake-claude".into())),
            PathBuf::from("/work/fake-claude")
        );
    }
}
