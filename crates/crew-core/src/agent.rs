//! Agent configuration.

use serde::{Deserialize, Serialize};

use crate::policy::HandoffPolicy;

/// Run time limit for agents that do not set one: none.
pub const DEFAULT_MAX_RUN_MINUTES: u32 = 0;

fn default_max_run_minutes() -> u32 {
    DEFAULT_MAX_RUN_MINUTES
}

/// Which CLI runs the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    ClaudeCode,
    /// Runs through `codex app-server`.
    Codex,
}

/// How much the agent may do without asking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    ReadOnly,
    EditFiles,
    /// Edits files and runs commands a safety check finds safe. Claude
    /// Code's `auto` mode; Codex asks only when it needs more access.
    Auto,
}

impl PermissionMode {
    /// Whether the agent may change files.
    pub fn edits(&self) -> bool {
        *self != PermissionMode::ReadOnly
    }
}

/// Whether Claude Code's auto mode works with `model`: Opus and Sonnet
/// 4.6 or newer, Haiku 5.5 or newer and any Fable model. The aliases
/// name current models. Ids it can't read count as supported: the CLI
/// says so if they aren't.
pub fn auto_supported(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    if model.contains("fable") {
        return true;
    }
    if model.contains("claude-3") {
        return false;
    }
    let model = model.split('[').next().unwrap_or_default();
    let parts: Vec<&str> = model.split(['-', '.', '/', '@', ':', '_']).collect();
    let Some(at) = parts
        .iter()
        .position(|p| matches!(*p, "opus" | "sonnet" | "haiku"))
    else {
        return true;
    };
    let number = |i: usize| {
        parts
            .get(i)
            .filter(|p| (1..=2).contains(&p.len()))
            .and_then(|p| p.parse::<u32>().ok())
    };
    let Some(major) = number(at + 1) else {
        // A bare alias such as `opus`.
        return true;
    };
    let minor = number(at + 2).unwrap_or(0);
    let least = if parts[at] == "haiku" { (5, 5) } else { (4, 6) };
    (major, minor) >= least
}

/// A chat member backed by an agent CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub role: String,
    pub runtime: Runtime,
    pub model: String,
    pub instructions: String,
    pub permission_mode: PermissionMode,
    pub allowed_tools: Vec<String>,
    /// Overrides the channel's handoff policy for this agent's requests.
    #[serde(default)]
    pub handoff_policy: Option<HandoffPolicy>,
    #[serde(default)]
    pub handoff_budget: Option<u32>,
    /// A run is stopped once it takes longer than this many minutes.
    /// 0 means no limit.
    #[serde(default = "default_max_run_minutes")]
    pub max_run_minutes: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_mode_reads_and_writes_as_auto() {
        assert_eq!(
            serde_json::to_string(&PermissionMode::Auto).unwrap(),
            r#""auto""#
        );
        let back: PermissionMode = serde_json::from_str(r#""edit_files""#).unwrap();
        assert_eq!(back, PermissionMode::EditFiles);
        assert!(PermissionMode::Auto.edits());
        assert!(!PermissionMode::ReadOnly.edits());
    }

    #[test]
    fn auto_needs_a_recent_model() {
        for model in [
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-sonnet-4-6[1m]",
            "claude-haiku-5-5",
            "claude-haiku-6",
            "claude-fable-1",
            "fable",
            "opus",
            "sonnet",
            "haiku",
            "sonnet[1m]",
            "us.anthropic.claude-sonnet-4-6-v1:0",
            "some-new-model",
        ] {
            assert!(auto_supported(model), "{model} should support auto");
        }
        for model in [
            "claude-opus-4-5",
            "claude-opus-4-1",
            "claude-opus-4",
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5",
            "claude-haiku-5",
            "claude-3-5-sonnet-20241022",
            "claude-3-opus-20240229",
        ] {
            assert!(!auto_supported(model), "{model} should not support auto");
        }
    }
}
