//! What you opened lately, for the palette's Recent section, and the
//! folders you picked lately, for Add project. Kept in the app prefs file
//! under `recent`.

use std::path::{Path, PathBuf};

use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_core::project_kind::ProjectKind;
use serde::{Deserialize, Serialize};

/// Threads kept, more than shown, so deleted or archived ones leave
/// enough behind.
pub const KEPT_THREADS: usize = 10;

/// Folders Add project lists under Recent folders.
pub const KEPT_FOLDERS: usize = 3;

const KEY: &str = "recent";

/// A folder you picked or added, and what it held then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentFolder {
    pub path: PathBuf,
    pub kind: ProjectKind,
}

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
    /// Folders picked or added, newest first.
    pub folders: Vec<RecentFolder>,
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

    /// Puts `path` first, with what it holds now. Whether anything
    /// changed.
    pub fn picked_folder(&mut self, path: &Path, kind: ProjectKind) -> bool {
        let folder = RecentFolder {
            path: path.to_path_buf(),
            kind,
        };
        if self.folders.first() == Some(&folder) {
            return false;
        }
        self.folders.retain(|f| f.path != folder.path);
        self.folders.insert(0, folder);
        self.folders.truncate(KEPT_FOLDERS);
        true
    }

    /// Whether anything changed.
    pub fn visited_agent(&mut self, agent: AgentId) -> bool {
        self.agent.replace(agent) != Some(agent)
    }
}

#[cfg(test)]
mod tests {
    use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};

    use std::path::Path;

    use crew_core::project_kind::ProjectKind;

    use super::{KEPT_FOLDERS, KEPT_THREADS, Recents};

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
    fn folders_keep_the_last_three_picked() {
        let mut r = Recents::default();
        assert!(r.picked_folder(Path::new("/work/a"), ProjectKind::Repo));
        assert!(!r.picked_folder(Path::new("/work/a"), ProjectKind::Repo));
        assert!(r.picked_folder(Path::new("/work/b"), ProjectKind::Plain));
        assert!(r.picked_folder(Path::new("/work/c"), ProjectKind::Repo));
        // Picked again, it moves up and takes what it holds now.
        assert!(r.picked_folder(Path::new("/work/a"), ProjectKind::Plain));
        assert!(r.picked_folder(Path::new("/work/d"), ProjectKind::Repo));
        let paths: Vec<_> = r.folders.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            ["/work/d", "/work/a", "/work/c"].map(Into::<std::path::PathBuf>::into)
        );
        assert_eq!(r.folders.len(), KEPT_FOLDERS);
        assert_eq!(r.folders[1].kind, ProjectKind::Plain);
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
        r.picked_folder(
            Path::new("/work/platform"),
            ProjectKind::MultiRepo {
                repos: vec!["api".into()],
            },
        );
        r.save(home.path()).unwrap();
        assert_eq!(Recents::load(home.path()), r);
        assert_eq!(
            crate::editor::load_choice(home.path()).as_deref(),
            Some("Zed")
        );
    }
}
