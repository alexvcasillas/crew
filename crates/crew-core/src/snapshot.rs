//! What an agent working in place found and left, so its changes can be
//! undone without touching yours.
//!
//! When a writing run starts in a project folder, Crew notes each repo's
//! `HEAD` and saves the content of every file that differs from it, yours
//! included, as git blobs. When the run ends, it saves the content of each
//! file the run edited or wrote. Undoing a file then puts back what it was
//! before the run, but only while it still is what the run left.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::worktree;

/// One repo as a run found it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStart {
    /// Relative to the project folder, empty for the folder itself.
    pub repo: String,
    pub head: String,
    /// Files that differed from `HEAD`, relative to the repo.
    pub dirty: Vec<DirtyFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyFile {
    pub path: String,
    /// The content's blob, `None` if the file was deleted.
    pub blob: Option<String>,
}

/// A run's record. Paths in `end` are relative to the project folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub repos: Vec<RepoStart>,
    /// Filled in once the run ended: each file it touched and its blob
    /// then, `None` if missing.
    #[serde(default)]
    pub end: Option<Vec<(String, Option<String>)>>,
}

/// A file as it was before a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Before {
    /// This content, as a blob.
    Content(String),
    /// As `HEAD` has it: the file was clean.
    Head,
    /// It wasn't there: the run made it, or you had deleted it.
    Absent,
}

impl RunSnapshot {
    /// The state of `path` (in `repo`) before the run, or `None` if the
    /// run didn't see the repo.
    pub fn before(&self, repo: &str, path: &str) -> Option<Before> {
        let start = self.repos.iter().find(|r| r.repo == repo)?;
        Some(match start.dirty.iter().find(|d| d.path == path) {
            Some(DirtyFile {
                blob: Some(blob), ..
            }) => Before::Content(blob.clone()),
            Some(DirtyFile { blob: None, .. }) => Before::Absent,
            None => Before::Head,
        })
    }

    /// The blob the run left at the folder-relative `path`, if it touched
    /// it and ended: `Some(None)` is a file it left missing.
    pub fn end_of(&self, path: &str) -> Option<Option<String>> {
        self.end
            .as_ref()?
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, blob)| blob.clone())
    }
}

/// What undoing one file comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undo {
    /// Put this back.
    To(Before),
    /// It changed after the run, so it is left as it is.
    ChangedAfter,
    /// Nothing records it from before the run.
    NoRecord,
}

/// Works out how to undo a file. `runs` are the runs that touched it,
/// newest first, each with its state before and the blob it left;
/// `current` is the blob on disk now (`None` if missing) and `head` the
/// blob in `HEAD` now. Runs are undone one by one while the file still is
/// what each left.
pub fn plan_undo(
    current: Option<&str>,
    head: Option<&str>,
    runs: &[(Option<Before>, Option<Option<String>>)],
) -> Undo {
    let mut now: Option<String> = current.map(str::to_string);
    let mut target: Option<Before> = None;
    for (before, end) in runs {
        let (Some(before), Some(end)) = (before, end) else {
            return match target {
                Some(t) => Undo::To(t),
                None => Undo::NoRecord,
            };
        };
        if *end != now {
            return match target {
                Some(t) => Undo::To(t),
                None => Undo::ChangedAfter,
            };
        }
        now = match before {
            Before::Content(blob) => Some(blob.clone()),
            Before::Head => head.map(str::to_string),
            Before::Absent => None,
        };
        target = Some(before.clone());
    }
    match target {
        Some(t) => Undo::To(t),
        None => Undo::NoRecord,
    }
}

/// Notes each repo of `repos` (relative to `root`) as it is now: its
/// `HEAD` and the content of every file that differs from it, saved as
/// blobs. A repo git can't read is left out.
pub fn take(root: &Path, repos: &[String]) -> RunSnapshot {
    let mut out = RunSnapshot::default();
    for repo in repos {
        let dir = root.join(repo);
        let Ok(head) = worktree::head_sha(&dir) else {
            continue;
        };
        let Ok(dirty) = worktree::changed_files(&dir) else {
            continue;
        };
        let Ok(blobs) = worktree::hash_files(&dir, &dirty, true) else {
            continue;
        };
        out.repos.push(RepoStart {
            repo: repo.clone(),
            head,
            dirty: dirty
                .into_iter()
                .zip(blobs)
                .map(|(path, blob)| DirtyFile { path, blob })
                .collect(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::snapshot::{Before, Undo, plan_undo};

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn undo_goes_back_while_the_file_is_what_each_run_left() {
        // One run over your own edit: back to your edit.
        let one = [(Some(Before::Content("yours".into())), Some(s("run")))];
        assert_eq!(
            plan_undo(Some("run"), Some("head"), &one),
            Undo::To(Before::Content("yours".into()))
        );
        // Changed after the run: left alone.
        assert_eq!(
            plan_undo(Some("later"), Some("head"), &one),
            Undo::ChangedAfter
        );
        // Two runs in a row on a clean file: back to HEAD.
        let two = [
            (Some(Before::Content("first".into())), Some(s("second"))),
            (Some(Before::Head), Some(s("first"))),
        ];
        assert_eq!(
            plan_undo(Some("second"), Some("head"), &two),
            Undo::To(Before::Head)
        );
        // You edited between the runs: only the newer run is undone.
        let between = [
            (Some(Before::Content("yours".into())), Some(s("second"))),
            (Some(Before::Head), Some(s("first"))),
        ];
        assert_eq!(
            plan_undo(Some("second"), Some("head"), &between),
            Undo::To(Before::Content("yours".into()))
        );
        // A file the run made, and one with no record.
        let made = [(Some(Before::Absent), Some(s("new")))];
        assert_eq!(
            plan_undo(Some("new"), None, &made),
            Undo::To(Before::Absent)
        );
        assert_eq!(
            plan_undo(Some("x"), None, &[(None, Some(s("x")))]),
            Undo::NoRecord
        );
        assert_eq!(plan_undo(Some("x"), None, &[]), Undo::NoRecord);
    }
}
