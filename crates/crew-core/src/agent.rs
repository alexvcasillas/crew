//! Agent configuration.

use serde::{Deserialize, Serialize};

/// Which CLI runs the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Runtime {
    ClaudeCode,
}

/// How much the agent may do without asking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}
