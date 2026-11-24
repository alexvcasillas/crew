//! What you opened lately, for the palette's Recent section. Kept in the
//! app prefs file under `recent`.

use std::path::Path;

use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use serde::{Deserialize, Serialize};

/// Threads kept, more than shown, so deleted or archived ones leave
/// enough behind.
pub const KEPT_THREADS: usize = 10;

const KEY: &str = "recent";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recents {
    /// Newest first.
    pub threads: Vec<ThreadId>,
    /// The channel you last went to.
    pub channel: Option<ChannelId>,
    /// The agent whose page you last opened.
    pub agent: Option<AgentId>,
    /// The project you last went to.
    pub project: Option<ProjectId>,
}

impl Recents {
    /// What the prefs file in `crew_home` holds, or nothing.
    pub fn load(crew_home: &Path) -> Recents {
        crate::prefs::read(crew_home, KEY)
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    /// Writes them to the prefs file. Blocks, so call it off the UI thread.
    pub fn save(&self, crew_home: &Path) -> std::io::Result<()> {
        let value = serde_json::to_value(self).map_err(std::io::Error::other)?;
        crate::prefs::write(crew_home, KEY, value)
    }

    /// Puts `thread` first. Whether anything changed.
    pub fn opened_thread(&mut self, thread: ThreadId) -> bool {
        if self.threads.first() == Some(&thread) {
            return false;
        }
        self.threads.retain(|t| *t != thread);
        self.threads.insert(0, thread);
        self.threads.truncate(KEPT_THREADS);
        true
    }

    /// Whether anything changed.
    pub fn visited_channel(&mut self, channel: ChannelId) -> bool {
        self.channel.replace(channel) != Some(channel)
    }

    /// Whether anything changed.
    pub fn visited_project(&mut self, project: ProjectId) -> bool {
        self.project.replace(project) != Some(project)
    }

    /// Whether anything changed.
    pub fn visited_agent(&mut self, agent: AgentId) -> bool {
        self.agent.replace(agent) != Some(agent)
    }
}

#[cfg(test)]
mod tests {
    use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};

    use super::{KEPT_THREADS, Recents};

    fn thread() -> ThreadId {
        ThreadId::new()
    }

    #[test]
    fn threads_are_newest_first_without_repeats() {
        let mut r = Recents::default();
        let (a, b) = (thread(), thread());
        assert!(r.opened_thread(a));
        assert!(r.opened_thread(b));
        assert!(!r.opened_thread(b), "already first");
        assert!(r.opened_thread(a));
        assert_eq!(r.threads, [a, b]);
        for _ in 0..20 {
            r.opened_thread(thread());
        }
        assert_eq!(r.threads.len(), KEPT_THREADS);
    }

    #[test]
    fn they_survive_a_restart_beside_other_prefs() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(Recents::load(home.path()), Recents::default());
        crate::editor::save_choice(home.path(), "Zed").unwrap();
        let mut r = Recents::default();
        r.opened_thread(thread());
        assert!(r.visited_channel(ChannelId::new()));
        assert!(r.visited_agent(AgentId::new()));
        let project = ProjectId::new();
        assert!(r.visited_project(project));
        assert!(!r.visited_project(project));
        r.save(home.path()).unwrap();
        assert_eq!(Recents::load(home.path()), r);
        assert_eq!(
            crate::editor::load_choice(home.path()).as_deref(),
            Some("Zed")
        );
    }
}
