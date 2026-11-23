//! Agent configuration.

use serde::{Deserialize, Serialize};

use crate::policy::HandoffPolicy;

/// Run time limit for agents that do not set one.
pub const DEFAULT_MAX_RUN_MINUTES: u32 = 20;

fn default_max_run_minutes() -> u32 {
    DEFAULT_MAX_RUN_MINUTES
}

/// Which CLI runs the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    ClaudeCode,
}

/// How much the agent may do without asking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    ReadOnly,
    EditFiles,
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
    /// A run is stopped once it takes longer than this.
    #[serde(default = "default_max_run_minutes")]
    pub max_run_minutes: u32,
}
