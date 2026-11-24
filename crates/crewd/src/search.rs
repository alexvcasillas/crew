//! Search for the palette: threads, channels, agents and files.
//!
//! Files come from git (`ls-files` and `status`), never from walking
//! folders, so ignored ones such as `node_modules` cost nothing. Each
//! checkout's list is cached. A thread's list is dropped and worked out
//! again in the background whenever its change counts may have changed:
//! a run ended, a commit, a discard or a worktree setup. A list older than
//! [`STALE`] is still used, and worked out again in the background.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crew_core::ids::{ProjectId, ThreadId};
use crew_core::store::{RunFilter, StoredProject};
use crew_core::worktree;
use crew_proto::{FileHit, SearchResults, SearchTotals, ThreadHit};

use crate::profile::thread_state;
use crate::service::{Reply, Service, blocking, json, log};

/// How old a cached file list may get before it is worked out again.
const STALE: Duration = Duration::from_secs(30);
/// Checkouts listed at once when a search finds them missing.
const LIST_AT_ONCE: usize = 8;
/// Most results per section.
const MAX_LIMIT: u32 = 200;

/// One file of a checkout, ready to match.
struct Listed {
    path: String,
    lower: String,
    /// Where the file name starts in `lower`.
    name_at: usize,
    changed: bool,
}

/// The files of one checkout.
pub(crate) struct Listing {
    files: Vec<Listed>,
}

impl Listing {
    fn from(list: worktree::FileList) -> Listing {
        let files = list
            .files
            .into_iter()
            .map(|path| {
                let lower = path.to_lowercase();
                let name_at = lower.rfind('/').map_or(0, |i| i + 1);
                Listed {
                    changed: list.changed.contains(&path),
                    path,
                    lower,
                    name_at,
                }
            })
            .collect();
        Listing { files }
    }

    fn empty() -> Listing {
        Listing { files: Vec::new() }
    }
}

#[derive(Default)]
struct Slot {
    list: Option<Arc<Listing>>,
    built: Option<Instant>,
    /// Grows when the checkout may have changed, so a list worked out
    /// before that isn't saved as current.
    generation: u64,
    refreshing: bool,
}

/// Cached file lists, by checkout folder.
#[derive(Default)]
pub(crate) struct FileIndex {
    slots: HashMap<PathBuf, Slot>,
}

impl FileIndex {
    /// Drops the folder's list: it may have changed.
    pub(crate) fn invalidate(&mut self, dir: &Path) {
        if let Some(slot) = self.slots.get_mut(dir) {
            slot.generation += 1;
            slot.list = None;
            slot.built = None;
        }
    }

    /// Forgets the folder altogether.
    pub(crate) fn forget(&mut self, dir: &Path) {
        self.slots.remove(dir);
    }

    fn generation(&self, dir: &Path) -> u64 {
        self.slots.get(dir).map_or(0, |s| s.generation)
    }

    /// Saves a list worked out at `generation`, unless the folder changed
    /// since. A background refresh ends either way.
    fn save(&mut self, dir: &Path, generation: u64, list: Arc<Listing>) {
        let slot = self.slots.entry(dir.to_path_buf()).or_default();
        slot.refreshing = false;
        if slot.generation == generation {
            slot.list = Some(list);
            slot.built = Some(Instant::now());
        }
    }
}

/// How well `needle` matches `text`, both lowercase: 0 at the start, 1 at
/// the start of a word, 2 anywhere else.
pub(crate) fn rank(text: &str, needle: &str) -> Option<u8> {
    if needle.is_empty() {
        return None;
    }
    if text.starts_with(needle) {
        return Some(0);
    }
    let mut best = None;
    for (i, _) in text.match_indices(needle) {
        if text[..i]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_alphanumeric())
        {
            return Some(1);
        }
        best = Some(2);
    }
    best
}

/// A file matches by its name first, else anywhere in its path.
fn file_rank(file: &Listed, needle: &str) -> Option<u8> {
    rank(&file.lower[file.name_at..], needle).or_else(|| rank(&file.lower, needle).map(|r| r + 3))
}

/// Lists the checkout's files, or none if it isn't one. Never fails.
fn list(dir: &Path) -> Listing {
    match worktree::list_files(dir) {
        Ok(list) => Listing::from(list),
        Err(e) => {
            if dir.join(".git").exists() {
                log(format!("couldn't list the files of {}: {e}", dir.display()));
            }
            Listing::empty()
        }
    }
}

/// One checkout to search: a project root or a thread's worktree.
struct Checkout {
    project: ProjectId,
    thread: Option<ThreadId>,
    dir: PathBuf,
    /// Files copied in from the root, which aren't the thread's work.
    skip: Vec<String>,
}

impl Service {
    /// Answers `Search`, off the async threads.
    pub(crate) async fn search(
        &self,
        query: String,
        project: Option<ProjectId>,
        limit: u32,
    ) -> Reply {
        let svc = self.clone();
        json(blocking(move || svc.search_now(&query, project, limit)).await?)
    }

    fn search_now(
        &self,
        query: &str,
        project: Option<ProjectId>,
        limit: u32,
    ) -> Result<SearchResults, String> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(SearchResults::default());
        }
        let limit = limit.clamp(1, MAX_LIMIT) as usize;
        let mut out = SearchResults::default();
        let mut totals = SearchTotals::default();

        let db = self.db();
        let err = |e: crew_core::store::StoreError| e.to_string();
        let projects: Vec<StoredProject> = match project {
            Some(p) => vec![db.store.project(p).map_err(err)?],
            None => db.store.projects().map_err(err)?,
        };

        // Threads: title matches, then message matches.
        let threads = db.store.open_threads(project).map_err(err)?;
        let mut hits: Vec<(u8, i64, usize)> = threads
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                rank(&t.title.to_lowercase(), &needle).map(|r| (r, -t.created_at, i))
            })
            .collect();
        let titled: HashSet<ThreadId> = hits.iter().map(|(_, _, i)| threads[*i].id).collect();
        let mentioned: HashSet<ThreadId> = db
            .store
            .threads_mentioning(&needle, project)
            .map_err(err)?
            .into_iter()
            .filter(|t| !titled.contains(t))
            .collect();
        hits.extend(
            threads
                .iter()
                .enumerate()
                .filter(|(_, t)| mentioned.contains(&t.id))
                .map(|(i, t)| (3, -t.created_at, i)),
        );
        hits.sort();
        totals.threads = count(hits.len());
        hits.truncate(limit);
        let waiting: HashSet<ThreadId> = db
            .store
            .pending_approvals(None)
            .map_err(err)?
            .into_iter()
            .map(|a| a.thread)
            .collect();
        for (_, _, i) in hits {
            let t = &threads[i];
            let runs = db
                .store
                .runs(RunFilter {
                    status: None,
                    thread: Some(t.id),
                })
                .map_err(err)?;
            out.threads.push(ThreadHit {
                thread: t.id,
                title: t.title.clone(),
                channel: t.channel,
                state: thread_state(&runs, waiting.contains(&t.id)),
            });
        }

        // Channels of the projects searched.
        let mut channels = Vec::new();
        let mut project_of = HashMap::new();
        for p in &projects {
            for c in db.store.channels(p.id).map_err(err)? {
                project_of.insert(c.id, p.id);
                if let Some(r) = rank(&c.name.to_lowercase(), &needle) {
                    channels.push((r, c.name.len(), c.name, c.id));
                }
            }
        }
        channels.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
        totals.channels = count(channels.len());
        out.channels = channels
            .into_iter()
            .take(limit)
            .map(|(.., id)| id)
            .collect();

        // Agents that aren't deleted, wherever they work.
        let mut agents: Vec<_> = db
            .store
            .agents()
            .map_err(err)?
            .into_iter()
            .filter_map(|(id, spec)| {
                rank(&spec.name.to_lowercase(), &needle)
                    .map(|r| (r, spec.name.len(), spec.name, id))
            })
            .collect();
        agents.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
        totals.agents = count(agents.len());
        out.agents = agents.into_iter().take(limit).map(|(.., id)| id).collect();
        drop(db);

        // Files: each root first, so a worktree's unchanged copy of a root
        // file is left out.
        let mut checkouts = Vec::new();
        for p in &projects {
            checkouts.push(Checkout {
                project: p.id,
                thread: None,
                dir: p.root.clone(),
                skip: Vec::new(),
            });
            for t in threads
                .iter()
                .filter(|t| project_of.get(&t.channel) == Some(&p.id))
            {
                if let Some(wt) = &t.worktree {
                    checkouts.push(Checkout {
                        project: p.id,
                        thread: Some(t.id),
                        dir: wt.path.clone(),
                        skip: p.copy_files.clone(),
                    });
                }
            }
        }
        checkouts.retain(|c| c.dir.is_dir());
        let lists = self.file_lists(&checkouts);
        let mut files: Vec<(u8, bool, usize, &str, FileHit)> = Vec::new();
        let mut seen: HashSet<(ProjectId, &str)> = HashSet::new();
        for (checkout, listing) in checkouts.iter().zip(&lists) {
            for f in &listing.files {
                let Some(r) = file_rank(f, &needle) else {
                    continue;
                };
                if checkout.thread.is_some() && checkout.skip.contains(&f.path) {
                    continue;
                }
                // A changed copy always shows; an unchanged one only if
                // no earlier checkout of the project had the file.
                let first = seen.insert((checkout.project, f.path.as_str()));
                if !f.changed && !first {
                    continue;
                }
                files.push((
                    r,
                    !f.changed,
                    f.path.len(),
                    f.path.as_str(),
                    FileHit {
                        project: checkout.project,
                        thread: checkout.thread,
                        path: f.path.clone(),
                        changed: f.changed,
                    },
                ));
            }
        }
        files.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));
        totals.files = count(files.len());
        out.files = files.into_iter().take(limit).map(|(.., hit)| hit).collect();
        out.totals = totals;
        Ok(out)
    }

    /// The file list of each checkout, from the cache where it has one.
    /// Missing ones are listed now, a few at a time; old ones are used and
    /// listed again in the background.
    fn file_lists(&self, checkouts: &[Checkout]) -> Vec<Arc<Listing>> {
        let mut out: Vec<Option<Arc<Listing>>> = vec![None; checkouts.len()];
        let mut missing: Vec<(usize, u64)> = Vec::new();
        {
            let mut index = self.files();
            for (i, c) in checkouts.iter().enumerate() {
                let slot = index.slots.entry(c.dir.clone()).or_default();
                match &slot.list {
                    Some(list) => {
                        out[i] = Some(list.clone());
                        let old = slot.built.is_none_or(|at| at.elapsed() >= STALE);
                        if old && !slot.refreshing {
                            slot.refreshing = true;
                            self.relist_in_background(c.dir.clone(), slot.generation);
                        }
                    }
                    None => missing.push((i, slot.generation)),
                }
            }
        }
        for chunk in missing.chunks(LIST_AT_ONCE) {
            let built: Vec<(usize, u64, Arc<Listing>)> = std::thread::scope(|s| {
                let jobs: Vec<_> = chunk
                    .iter()
                    .map(|&(i, generation)| {
                        let dir = &checkouts[i].dir;
                        s.spawn(move || (i, generation, Arc::new(list(dir))))
                    })
                    .collect();
                jobs.into_iter()
                    .map(|j| {
                        j.join()
                            .unwrap_or_else(|_| (0, u64::MAX, Arc::new(Listing::empty())))
                    })
                    .collect()
            });
            let mut index = self.files();
            for (i, generation, listing) in built {
                if generation != u64::MAX {
                    index.save(&checkouts[i].dir, generation, listing.clone());
                    out[i] = Some(listing);
                }
            }
        }
        out.into_iter()
            .map(|l| l.unwrap_or_else(|| Arc::new(Listing::empty())))
            .collect()
    }

    /// Lists the folder again off the async threads and saves the list
    /// unless the folder changed meanwhile. Without a runtime it is left
    /// for the next search.
    fn relist_in_background(&self, dir: PathBuf, generation: u64) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let svc = self.clone();
        rt.spawn_blocking(move || {
            if !dir.is_dir() {
                svc.files().forget(&dir);
                return;
            }
            let listing = Arc::new(list(&dir));
            svc.files().save(&dir, generation, listing);
        });
    }

    /// Drops the cached file list of the thread's worktree and lists it
    /// again in the background.
    pub(crate) fn files_may_differ(&self, thread: ThreadId) {
        let dir = match self.db().store.thread(thread) {
            Ok(t) => t.worktree.map(|w| w.path),
            Err(_) => None,
        };
        let Some(dir) = dir else { return };
        let generation = {
            let mut index = self.files();
            index.invalidate(&dir);
            let Some(slot) = index.slots.get_mut(&dir) else {
                return;
            };
            if slot.refreshing {
                return;
            }
            slot.refreshing = true;
            index.generation(&dir)
        };
        self.relist_in_background(dir, generation);
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use crate::search::rank;

    #[test]
    fn matches_rank_start_then_word_start_then_anywhere() {
        assert_eq!(rank("parser crash", "parse"), Some(0));
        assert_eq!(rank("fix the parser", "parse"), Some(1));
        assert_eq!(rank("src/parser.rs", "parse"), Some(1));
        assert_eq!(rank("speed up the reparse", "parse"), Some(2));
        // A later word start beats an earlier match inside a word.
        assert_eq!(rank("reparse the parser", "parse"), Some(1));
        assert_eq!(rank("lexer", "parse"), None);
        assert_eq!(rank("anything", ""), None);
        assert_eq!(rank("café au lait", "au"), Some(1));
    }
}
