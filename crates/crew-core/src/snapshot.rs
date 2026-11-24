//! What an agent working in place found and left, so its changes can be
//! undone without touching yours.
//!
//! When a writing run starts in a project folder, Crew notes each repo's
//! `HEAD` and keeps a copy of each file that differs from it, yours
//! included, in a store of its own (never only in git, whose cleanup could
//! drop it). Copies are named by their git blob id, worked out from the
//! copy itself, and shared between runs. When the run ends, Crew notes the
//! id of each file the run edited or wrote. Undoing a file then puts back
//! what it was before the run, but only while it still is what the run
//! left.

use std::path::{Path, PathBuf};

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
    /// The content's blob id, `None` if the file was deleted.
    pub blob: Option<String>,
    /// Whether a copy is in the store, else why not.
    #[serde(default)]
    pub copy: Copy,
    /// Permission bits, to put back with the content.
    #[serde(default)]
    pub mode: Option<u32>,
}

/// Whether a dirty file's content was kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Copy {
    Kept,
    /// Over a size limit: only its id is known.
    TooBig,
    /// Copying it failed: only its id, if any, is known.
    #[default]
    Failed,
}

/// How much a snapshot copies, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Largest tracked file copied.
    pub file: u64,
    /// Largest untracked file copied.
    pub untracked: u64,
    /// Most copied for one run, all files together.
    pub total: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            file: 20 * 1024 * 1024,
            untracked: 1024 * 1024,
            total: 200 * 1024 * 1024,
        }
    }
}

/// Whether `id` is a git blob id: 40 or 64 lowercase hex digits. Only
/// such ids are used as file names.
pub fn valid_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Where the copy of `id` is kept in the store, if `id` is valid.
pub fn copy_path(store: &Path, id: &str) -> Option<PathBuf> {
    valid_id(id).then(|| store.join(id))
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

/// Content a file had before a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub blob: String,
    pub copy: Copy,
    pub mode: Option<u32>,
}

/// A file as it was before a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Before {
    /// This content.
    Content(Kept),
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
                blob: Some(blob),
                copy,
                mode,
                ..
            }) => Before::Content(Kept {
                blob: blob.clone(),
                copy: *copy,
                mode: *mode,
            }),
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

    /// The ids of the copies this record points to.
    pub fn copies(&self) -> impl Iterator<Item = &str> {
        self.repos
            .iter()
            .flat_map(|r| &r.dirty)
            .filter(|d| d.copy == Copy::Kept)
            .filter_map(|d| d.blob.as_deref())
    }

    /// Dirty files whose content couldn't be kept.
    pub fn skipped(&self) -> usize {
        self.repos
            .iter()
            .flat_map(|r| &r.dirty)
            .filter(|d| d.blob.is_some() && d.copy != Copy::Kept)
            .count()
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
            Before::Content(kept) => Some(kept.blob.clone()),
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
/// `HEAD` and each file that differs from it: tracked ones, and untracked
/// ones git doesn't ignore. Contents go to `store` (made 0700, each copy
/// 0600) within `caps`; a file past a cap keeps only its id. A repo git
/// can't read is left out.
pub fn take(root: &Path, repos: &[String], store: &Path, caps: Caps) -> RunSnapshot {
    use std::os::unix::fs::PermissionsExt;
    let mut out = RunSnapshot::default();
    let can_copy = std::fs::create_dir_all(store)
        .and_then(|()| std::fs::set_permissions(store, std::fs::Permissions::from_mode(0o700)))
        .is_ok();
    let mut total = 0u64;
    for repo in repos {
        let dir = root.join(repo);
        let Ok(head) = worktree::head_sha(&dir) else {
            continue;
        };
        let (Ok(changed), Ok(untracked)) = (
            worktree::changed_files(&dir),
            worktree::untracked_files(&dir),
        ) else {
            continue;
        };
        let mut dirty = Vec::new();
        for path in changed {
            let file = dir.join(&path);
            let Ok(meta) = std::fs::symlink_metadata(&file) else {
                // Deleted: nothing to copy.
                dirty.push(DirtyFile {
                    path,
                    blob: None,
                    copy: Copy::Kept,
                    mode: None,
                });
                continue;
            };
            let mode = Some(meta.permissions().mode() & 0o7777);
            let cap = if untracked.contains(&path) {
                caps.untracked
            } else {
                caps.file
            };
            let fits = meta.is_file() && meta.len() <= cap && total + meta.len() <= caps.total;
            let kept = if can_copy && fits {
                keep_copy(&dir, &file, store).ok()
            } else {
                None
            };
            let (blob, copy) = match kept {
                Some(id) => {
                    total += meta.len();
                    (Some(id), Copy::Kept)
                }
                None => {
                    let id = worktree::hash_files(&dir, std::slice::from_ref(&path), false)
                        .ok()
                        .and_then(|mut v| v.pop())
                        .flatten();
                    let why = if can_copy && meta.is_file() && !fits {
                        Copy::TooBig
                    } else {
                        Copy::Failed
                    };
                    (id, why)
                }
            };
            dirty.push(DirtyFile {
                path,
                blob,
                copy,
                mode,
            });
        }
        out.repos.push(RepoStart {
            repo: repo.clone(),
            head,
            dirty,
        });
    }
    out
}

/// Copies `file` into `store` and names the copy by its own blob id, so
/// the copy always matches its name. Returns the id.
fn keep_copy(dir: &Path, file: &Path, store: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = store.join(format!(".copy-{}-{nanos}", std::process::id()));
    let copied = (|| {
        std::fs::copy(file, &tmp)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        let id = id_of(dir, &tmp)?;
        let to = copy_path(store, &id)
            .ok_or_else(|| std::io::Error::other(format!("odd blob id {id}")))?;
        if to.exists() {
            std::fs::remove_file(&tmp)?;
        } else {
            std::fs::rename(&tmp, &to)?;
        }
        Ok(id)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    copied
}

/// The blob id of the file at `path`, hashed with `dir`'s repo settings.
pub fn id_of(dir: &Path, path: &Path) -> std::io::Result<String> {
    let text = path.to_string_lossy().into_owned();
    worktree::hash_files(dir, &[text], false)
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .pop()
        .flatten()
        .ok_or_else(|| std::io::Error::other("the copy is gone"))
}

#[cfg(test)]
mod tests {
    use crate::snapshot::{Before, Copy, Kept, Undo, plan_undo, valid_id};

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    fn content(blob: &str) -> Before {
        Before::Content(Kept {
            blob: blob.into(),
            copy: Copy::Kept,
            mode: None,
        })
    }

    #[test]
    fn undo_goes_back_while_the_file_is_what_each_run_left() {
        // One run over your own edit: back to your edit.
        let one = [(Some(content("yours")), Some(s("run")))];
        assert_eq!(
            plan_undo(Some("run"), Some("head"), &one),
            Undo::To(content("yours"))
        );
        // Changed after the run: left alone.
        assert_eq!(
            plan_undo(Some("later"), Some("head"), &one),
            Undo::ChangedAfter
        );
        // Two runs in a row on a clean file: back to HEAD.
        let two = [
            (Some(content("first")), Some(s("second"))),
            (Some(Before::Head), Some(s("first"))),
        ];
        assert_eq!(
            plan_undo(Some("second"), Some("head"), &two),
            Undo::To(Before::Head)
        );
        // You edited between the runs: only the newer run is undone.
        let between = [
            (Some(content("yours")), Some(s("second"))),
            (Some(Before::Head), Some(s("first"))),
        ];
        assert_eq!(
            plan_undo(Some("second"), Some("head"), &between),
            Undo::To(content("yours"))
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

    #[test]
    fn only_hex_ids_name_copies() {
        assert!(valid_id(&"a1".repeat(20)));
        assert!(valid_id(&"0f".repeat(32)));
        assert!(!valid_id(&"A1".repeat(20)));
        assert!(!valid_id("../../etc/passwd"));
        assert!(!valid_id(&format!("{}/", "a".repeat(39))));
        assert!(!valid_id(""));
    }
}
