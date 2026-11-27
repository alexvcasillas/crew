use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Creates a new random id.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// Identifies a project.
    ProjectId
);
id_type!(
    /// Identifies a channel.
    ChannelId
);
id_type!(
    /// Identifies an agent.
    AgentId
);
id_type!(
    /// Identifies a thread.
    ThreadId
);
id_type!(
    /// Identifies a message.
    MessageId
);
id_type!(
    /// Identifies a scheduled job.
    JobId
);
id_type!(
    /// Identifies an agent run.
    RunId
);
id_type!(
    /// Identifies an approval request.
    ApprovalId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_serialize_as_plain_uuid() {
        let a = RunId::new();
        assert_ne!(a, RunId::new());
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, format!("\"{a}\""));
        assert_eq!(serde_json::from_str::<RunId>(&json).unwrap(), a);
    }
}
