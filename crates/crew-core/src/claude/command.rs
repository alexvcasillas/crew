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
        prompt,
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

    fn has_pair(args: &[String], k: &str, v: &str) -> bool {
        args.windows(2).any(|w| w[0] == k && w[1] == v)
    }

    #[test]
    fn read_only_agent_runs_in_plan_mode() {
        let inv = build_claude(
            &agent(PermissionMode::ReadOnly),
            "hi",
            Path::new("/work/demo"),
            SessionArg::New(Uuid::new_v4()),
        );
        assert!(has_pair(&inv.args, "--permission-mode", "plan"));
        assert!(has_pair(&inv.args, "--permission-prompts", "none"));
        assert_eq!(&inv.args[..2], ["-p", "hi"]);
        assert_eq!(inv.cwd, Path::new("/work/demo"));
    }

    #[test]
    fn edit_agent_accepts_edits() {
        let inv = build_claude(
            &agent(PermissionMode::EditFiles),
            "hi",
            Path::new("/work/demo"),
            SessionArg::New(Uuid::new_v4()),
        );
        assert!(has_pair(&inv.args, "--permission-mode", "acceptEdits"));
        assert!(has_pair(&inv.args, "--allowedTools", "Read,Grep"));
        assert!(has_pair(&inv.args, "--append-system-prompt", "Be brief."));
    }

    #[test]
    fn new_session_passes_crew_chosen_id() {
        let id = Uuid::new_v4();
        let inv = build_claude(
            &agent(PermissionMode::ReadOnly),
            "hi",
            Path::new("/work/demo"),
            SessionArg::New(id),
        );
        assert!(has_pair(&inv.args, "--session-id", &id.to_string()));
        assert!(!inv.args.contains(&"--resume".to_string()));
    }

    #[test]
    fn resume_uses_resume_flag_not_session_id() {
        let inv = build_claude(
            &agent(PermissionMode::ReadOnly),
            "hi",
            Path::new("/work/demo"),
            SessionArg::Resume("abc".into()),
        );
        assert!(has_pair(&inv.args, "--resume", "abc"));
        assert!(!inv.args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn empty_instructions_and_tools_are_omitted() {
        let mut a = agent(PermissionMode::ReadOnly);
        a.instructions.clear();
        a.allowed_tools.clear();
        let inv = build_claude(
            &a,
            "hi",
            Path::new("/work/demo"),
            SessionArg::New(Uuid::new_v4()),
        );
        assert!(!inv.args.contains(&"--append-system-prompt".to_string()));
        assert!(!inv.args.contains(&"--allowedTools".to_string()));
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
