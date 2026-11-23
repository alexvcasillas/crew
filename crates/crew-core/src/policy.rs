//! Handoff policy: when an agent may bring in another agent without asking.

use crate::mention::parse_mentions;
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

/// Budget used when no layer sets one.
const DEFAULT_BUDGET: u32 = 3;

/// One layer's setting: a policy and a budget, each optional.
pub type Layer = (Option<HandoffPolicy>, Option<u32>);

/// Everything `decide` needs to know about one handoff request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyInputs {
    pub thread: Layer,
    pub agent: Layer,
    pub channel: Layer,
    /// Agent-initiated handoffs already run in this thread.
    pub used: u32,
}

/// What to do with a handoff request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Ask the user first.
    Ask,
    /// Run it now.
    Run,
    /// The budget is used up, so wait for the user.
    Pause { budget: u32 },
}

/// Decides a handoff from the thread, agent and channel settings.
pub fn decide(i: &PolicyInputs) -> Decision {
    let first = i
        .thread
        .0
        .or(i.channel.0)
        .unwrap_or(HandoffPolicy::AskFirst);
    let policy = match first {
        HandoffPolicy::PerAgent => match i.agent.0 {
            Some(HandoffPolicy::PerAgent) | None => HandoffPolicy::AskFirst,
            Some(p) => p,
        },
        p => p,
    };
    match policy {
        HandoffPolicy::Budget => {
            let budget = i
                .thread
                .1
                .or(i.agent.1)
                .or(i.channel.1)
                .unwrap_or(DEFAULT_BUDGET);
            if i.used < budget {
                Decision::Run
            } else {
                Decision::Pause { budget }
            }
        }
        _ => Decision::Ask,
    }
}

/// Finds `@Agent` mentions in a reply, skipping `self_name`.
pub fn mentions_in_reply(text: &str, agents: &[&str], self_name: &str) -> Vec<String> {
    parse_mentions(text, agents)
        .0
        .into_iter()
        .filter(|n| !n.eq_ignore_ascii_case(self_name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use HandoffPolicy::*;

    fn inputs() -> PolicyInputs {
        PolicyInputs {
            thread: (None, None),
            agent: (None, None),
            channel: (None, None),
            used: 0,
        }
    }

    #[test]
    fn default_is_ask() {
        assert_eq!(decide(&inputs()), Decision::Ask);
    }

    #[test]
    fn thread_beats_channel() {
        let mut i = inputs();
        i.channel.0 = Some(Budget);
        i.thread.0 = Some(AskFirst);
        assert_eq!(decide(&i), Decision::Ask);
        i.thread.0 = Some(Budget);
        i.channel.0 = Some(AskFirst);
        assert_eq!(decide(&i), Decision::Run);
    }

    #[test]
    fn channel_applies_when_thread_unset() {
        let mut i = inputs();
        i.channel.0 = Some(Budget);
        assert_eq!(decide(&i), Decision::Run);
    }

    #[test]
    fn agent_policy_ignored_unless_per_agent() {
        let mut i = inputs();
        i.agent.0 = Some(Budget);
        assert_eq!(decide(&i), Decision::Ask);
    }

    #[test]
    fn per_agent_uses_agent_policy() {
        let mut i = inputs();
        i.channel.0 = Some(PerAgent);
        i.agent.0 = Some(Budget);
        assert_eq!(decide(&i), Decision::Run);
        i.thread.0 = Some(PerAgent);
        i.channel.0 = Some(Budget);
        i.agent.0 = Some(AskFirst);
        assert_eq!(decide(&i), Decision::Ask);
    }

    #[test]
    fn per_agent_without_agent_policy_asks() {
        let mut i = inputs();
        i.thread.0 = Some(PerAgent);
        assert_eq!(decide(&i), Decision::Ask);
        i.agent.0 = Some(PerAgent);
        assert_eq!(decide(&i), Decision::Ask);
    }

    #[test]
    fn budget_defaults_to_three() {
        let mut i = inputs();
        i.thread.0 = Some(Budget);
        i.used = 2;
        assert_eq!(decide(&i), Decision::Run);
        i.used = 3;
        assert_eq!(decide(&i), Decision::Pause { budget: 3 });
    }

    #[test]
    fn budget_layers_thread_agent_channel() {
        let mut i = inputs();
        i.thread.0 = Some(Budget);
        i.channel.1 = Some(5);
        i.used = 4;
        assert_eq!(decide(&i), Decision::Run);
        i.agent.1 = Some(2);
        assert_eq!(decide(&i), Decision::Pause { budget: 2 });
        i.thread.1 = Some(6);
        i.used = 5;
        assert_eq!(decide(&i), Decision::Run);
    }

    #[test]
    fn budget_zero_pauses_at_once() {
        let mut i = inputs();
        i.thread = (Some(Budget), Some(0));
        assert_eq!(decide(&i), Decision::Pause { budget: 0 });
    }

    #[test]
    fn reply_mentions_exclude_self() {
        let agents = ["Forge", "Triage", "Scout"];
        let got = mentions_in_reply(
            "@triage take this, @FORGE too, @Scout and @triage",
            &agents,
            "forge",
        );
        assert_eq!(got, vec!["Triage", "Scout"]);
        assert!(mentions_in_reply("@Forge only", &agents, "Forge").is_empty());
    }
}
