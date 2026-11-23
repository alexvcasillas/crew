//! Handoff policy: when an agent may bring in another agent without asking.

use serde::{Deserialize, Serialize};

/// How handoffs between agents are approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffPolicy {
    /// Every handoff waits for you.
    AskFirst,
    /// Handoffs run on their own until the budget is used up.
    Budget,
    /// The requesting agent's own setting decides.
    PerAgent,
}
