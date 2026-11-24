//! Terminal sessions: Claude Code sessions people run in their own
//! terminals, read from the claude home and shown under their project.
//!
//! Read-only. Crew never writes under the claude home, resumes these
//! sessions or sends them input.
//!
//! A watcher task scans the session files under `<claude home>/projects`.
//! It is woken by file events, with a poll as backup, and reads the
//! `<claude home>/sessions` registry for whether a session works now. It
//! never holds the service locks while it reads files.
//!
//! A scan reads at most [`MAX_READ`] bytes over all files, and leaves the
//! rest for the next one. Unless someone subscribes, a session costs only
//! a read of its first [`HEAD_BYTES`], for its cwd and title. The last
//! lines are read when someone subscribes, and new lines are read only
//! while someone does.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crew_core::claude::transcript::{self, TranscriptItem};
use crew_core::ids::ProjectId;
use crew_proto::{ServerMsg, TerminalSession};
use notify::Watcher;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::service::{Backlog, Service, log};

/// Lines read from the end of a file for a backlog, and items kept.
pub const TAIL_LINES: usize = 2000;
/// Most bytes one scan reads over all files, and most read at once.
pub const MAX_READ: u64 = 8 * 1024 * 1024;
/// Most bytes read from the start of a file for its cwd and title.
pub const HEAD_BYTES: u64 = 64 * 1024;
/// Sessions not written to for this long are not listed.
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Most bytes read of one registry entry.
const REGISTRY_ENTRY_BYTES: u64 = 64 * 1024;
/// Bytes read at a time when looking back for line starts.
const CHUNK: usize = 64 * 1024;
/// Poll interval when file events work, and how often the registry is read.
const SLOW_POLL: Duration = Duration::from_secs(5);
/// Poll interval when file events don't work.
const FAST_POLL: Duration = Duration::from_secs(1);
/// Without a registry, a session written to this recently is live.
const RECENT: Duration = Duration::from_secs(120);
/// How long file events settle before a scan.
const SETTLE: Duration = Duration::from_millis(50);

/// What the watcher shares with the service.
#[derive(Default)]
pub(crate) struct TerminalState {
    sessions: HashMap<String, Entry>,
}

struct Entry {
    info: TerminalSession,
    /// Seq of the next item kept.
    next_seq: u64,
    /// The last items with their seqs, kept while someone subscribes.
    buffer: Option<VecDeque<(u64, TranscriptItem)>>,
    subscribers: usize,
}

impl Entry {
    /// Keeps the last items of `items` as the backlog, numbered on from
    /// the last seq handed out.
    fn fill(&mut self, items: Vec<TranscriptItem>) {
        let skip = items.len().saturating_sub(TAIL_LINES);
        let mut buffer = VecDeque::with_capacity(items.len() - skip);
        for item in items.into_iter().skip(skip) {
            buffer.push_back((self.next_seq, item));
            self.next_seq += 1;
        }
        self.buffer = Some(buffer);
    }
}

/// Requests to the watcher task.
pub(crate) enum Cmd {
    /// Projects changed: match sessions again.
    Rescan,
    /// Someone subscribes to a session: read its backlog if needed.
    Load {
        session: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

impl TerminalState {
    /// Every session, most recently active first.
    pub(crate) fn sessions(&self) -> Vec<TerminalSession> {
        let mut all: Vec<TerminalSession> =
            self.sessions.values().map(|e| e.info.clone()).collect();
        all.sort_by(|a, b| {
            b.last_active_at
                .cmp(&a.last_active_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        all
    }

    /// The buffered items of a session as messages.
    pub(crate) fn backlog(&self, session: &str) -> Vec<ServerMsg> {
        let Some(buffer) = self.sessions.get(session).and_then(|e| e.buffer.as_ref()) else {
            return vec![];
        };
        buffer
            .iter()
            .map(|(seq, item)| ServerMsg::TerminalEvent {
                session: session.to_string(),
                seq: *seq,
                item: item.clone(),
            })
            .collect()
    }

    pub(crate) fn unsubscribe(&mut self, session: &str) {
        if let Some(entry) = self.sessions.get_mut(session) {
            entry.subscribers = entry.subscribers.saturating_sub(1);
            if entry.subscribers == 0 {
                entry.buffer = None;
            }
        }
    }

    /// Sessions whose new lines are read.
    fn buffered(&self) -> HashSet<String> {
        self.sessions
            .iter()
            .filter(|(_, e)| e.buffer.is_some())
            .map(|(id, _)| id.clone())
            .collect()
    }
}

impl Service {
    /// Starts the watcher on `claude_home`. It ends when `stop` fires or
    /// its sender is dropped.
    pub fn watch_terminal(
        &self,
        claude_home: PathBuf,
        stop: oneshot::Receiver<()>,
    ) -> JoinHandle<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.set_terminal_cmd(tx);
        let service = self.clone();
        tokio::spawn(watch(service, claude_home, rx, stop))
    }

    /// Makes sure the session's backlog is kept and counts one more
    /// subscriber. Pair each success with [`Service::unsubscribe_terminal`].
    pub async fn subscribe_terminal(&self, session: &str) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.send_terminal_cmd(Cmd::Load {
            session: session.to_string(),
            reply,
        })?;
        answer
            .await
            .map_err(|_| "Terminal sessions stopped.".to_string())?
    }

    /// The buffered items of a session and the serial they cover.
    pub fn terminal_backlog(&self, session: &str) -> Backlog {
        let db = self.db();
        let msgs = self.terminal().backlog(session);
        Backlog {
            msgs,
            serial: db.serial(),
        }
    }

    /// One subscriber less. The backlog goes once nobody subscribes.
    pub fn unsubscribe_terminal(&self, session: &str) {
        self.terminal().unsubscribe(session);
    }

    /// Tells the watcher the projects changed.
    pub(crate) fn rescan_terminal(&self) {
        let _ = self.send_terminal_cmd(Cmd::Rescan);
    }

    /// Records what a scan found and tells clients.
    fn apply(&self, updates: Vec<Update>, removed: Vec<String>) {
        if updates.is_empty() && removed.is_empty() {
            return;
        }
        let mut db = self.db();
        let mut terminal = self.terminal();
        for id in removed {
            if terminal.sessions.remove(&id).is_some() {
                self.publish_global(&mut db, ServerMsg::TerminalSessionRemoved { id });
            }
        }
        for u in updates {
            let id = u.info.id.clone();
            let entry = terminal
                .sessions
                .entry(id.clone())
                .or_insert_with(|| Entry {
                    info: u.info.clone(),
                    next_seq: 0,
                    buffer: None,
                    subscribers: 0,
                });
            if let Some(items) = u.reset
                && entry.buffer.is_some()
            {
                // Subscribers ask again for the new backlog.
                entry.fill(items);
                self.publish_global(
                    &mut db,
                    ServerMsg::TerminalSessionReset {
                        session: id.clone(),
                    },
                );
            }
            if let Some(buffer) = entry.buffer.as_mut() {
                for item in u.items {
                    let seq = entry.next_seq;
                    entry.next_seq += 1;
                    if buffer.len() == TAIL_LINES {
                        buffer.pop_front();
                    }
                    buffer.push_back((seq, item.clone()));
                    self.publish_global(
                        &mut db,
                        ServerMsg::TerminalEvent {
                            session: id.clone(),
                            seq,
                            item,
                        },
                    );
                }
            }
            if u.announce || entry.info != u.info {
                entry.info = u.info.clone();
                self.publish_global(
                    &mut db,
                    ServerMsg::TerminalSessionChanged { session: u.info },
                );
            }
        }
    }
}

/// What a scan found about one session.
struct Update {
    info: TerminalSession,
    /// New items, for subscribers.
    items: Vec<TranscriptItem>,
    /// The file was cut short or replaced: its new backlog.
    reset: Option<Vec<TranscriptItem>>,
    /// Seen for the first time: tell clients even if nothing changed.
    announce: bool,
}

async fn watch(
    service: Service,
    claude_home: PathBuf,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    mut stop: oneshot::Receiver<()>,
) {
    let projects_dir = claude_home.join("projects");
    let (event_tx, mut events) = mpsc::unbounded_channel::<()>();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = event_tx.send(());
        }
    })
    .and_then(|mut w| {
        w.watch(&projects_dir, notify::RecursiveMode::Recursive)?;
        Ok(w)
    });
    let poll = match &watcher {
        Ok(_) => SLOW_POLL,
        Err(e) => {
            log(format!(
                "can't watch {} ({e}), checking it every second",
                projects_dir.display()
            ));
            FAST_POLL
        }
    };
    let mut ticks = tokio::time::interval(poll);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut scanner = Some(Scanner::new(claude_home, service.worktrees().to_path_buf()));
    loop {
        let load = tokio::select! {
            _ = &mut stop => break,
            Some(()) = events.recv() => {
                tokio::time::sleep(SETTLE).await;
                while events.try_recv().is_ok() {}
                None
            }
            _ = ticks.tick() => None,
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Rescan) => None,
                Some(Cmd::Load { session, reply }) => Some((session, reply)),
                None => break,
            },
        };
        let Some(mut s) = scanner.take() else { break };
        let svc = service.clone();
        let done = tokio::task::spawn_blocking(move || {
            let more = match load {
                Some((session, reply)) => {
                    let _ = reply.send(s.load(&svc, &session));
                    false
                }
                None => s.scan(&svc),
            };
            (s, more)
        })
        .await;
        match done {
            Ok((s, more)) => {
                scanner = Some(s);
                if more {
                    // Over this scan's read budget: go on soon.
                    ticks.reset_after(Duration::from_millis(10));
                }
            }
            Err(e) => {
                log(format!("terminal sessions stopped: {e}"));
                break;
            }
        }
    }
    drop(watcher);
}

/// Where a session file stands.
enum Kind {
    /// No line with a cwd yet.
    Unknown,
    /// Not a terminal session: a headless run or one of Crew's.
    Skip,
    /// A terminal session in `cwd`.
    Known { cwd: String, real: PathBuf },
}

struct FileState {
    session: String,
    kind: Kind,
    /// File length when the head was last read.
    head_len: u64,
    /// Identity and read position since the last backlog read.
    ino: u64,
    /// Next byte to read. Always at a line start, or inside an over-long
    /// line being skipped.
    offset: u64,
    /// Inside a line too long to read: drop bytes up to its end.
    skipping: bool,
    title: Option<String>,
    started_at: Option<i64>,
    mtime: SystemTime,
    /// Clients were told about it.
    announced: bool,
}

/// One registry entry of a running Claude Code.
struct Running {
    pid: i32,
    status: Option<String>,
}

/// What one scan may still read.
struct Budget {
    left: u64,
    /// Bytes read so far.
    read: u64,
}

impl Budget {
    fn spend(&mut self, n: u64) {
        self.left = self.left.saturating_sub(n);
        self.read += n;
    }
}

pub(crate) struct Scanner {
    claude_home: PathBuf,
    worktrees: PathBuf,
    files: HashMap<PathBuf, FileState>,
    /// By session id. `None` if there is no registry folder.
    registry: Option<HashMap<String, Running>>,
    registry_read: Option<Instant>,
    /// Bytes read by scans so far.
    pub(crate) bytes_read: u64,
}

/// A project root as given and as resolved.
struct Root {
    id: ProjectId,
    paths: Vec<PathBuf>,
    prefixes: Vec<String>,
}

impl Scanner {
    /// `worktrees` is the crew home's worktree folder, with the home
    /// resolved.
    pub(crate) fn new(claude_home: PathBuf, worktrees: PathBuf) -> Scanner {
        Scanner {
            claude_home,
            worktrees,
            files: HashMap::new(),
            registry: None,
            registry_read: None,
            bytes_read: 0,
        }
    }

    fn roots(svc: &Service) -> Vec<Root> {
        let projects = match svc.db().store.projects() {
            Ok(p) => p,
            Err(e) => {
                log(format!("terminal sessions can't list projects: {e}"));
                return vec![];
            }
        };
        projects
            .into_iter()
            .map(|p| {
                let mut paths = vec![p.root.clone()];
                let real = real_path(&p.root);
                if real != p.root {
                    paths.push(real);
                }
                let prefixes = paths.iter().map(|p| folder_name(p)).collect();
                Root {
                    id: p.id,
                    paths,
                    prefixes,
                }
            })
            .collect()
    }

    /// Reads what changed, within the read budget, and tells clients.
    /// Returns whether something was left for the next scan.
    pub(crate) fn scan(&mut self, svc: &Service) -> bool {
        if self
            .registry_read
            .is_none_or(|at| at.elapsed() >= SLOW_POLL)
        {
            self.read_registry();
        }
        let roots = Self::roots(svc);
        let buffered = svc.terminal().buffered();
        let mut budget = Budget {
            left: MAX_READ,
            read: 0,
        };
        let mut updates = Vec::new();
        let mut more = false;
        let mut present = HashSet::new();
        for path in self.session_files(&roots) {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            if mtime.elapsed().is_ok_and(|age| age > MAX_AGE) {
                continue;
            }
            present.insert(path.clone());
            match self.check(svc, &roots, &path, &meta, &buffered, &mut budget) {
                Ok((update, rest)) => {
                    updates.extend(update);
                    more |= rest;
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::NotFound {
                        log(format!("can't read {}: {e}", path.display()));
                    }
                }
            }
        }
        self.bytes_read += budget.read;
        // Deleted, aged out or no longer in a project folder.
        let mut removed = Vec::new();
        self.files.retain(|path, f| {
            let keep = present.contains(path);
            if !keep && f.announced {
                removed.push(f.session.clone());
            }
            keep
        });
        svc.apply(updates, removed);
        more
    }

    /// Session files in folders that may belong to a project.
    fn session_files(&self, roots: &[Root]) -> Vec<PathBuf> {
        if roots.is_empty() {
            return vec![];
        }
        let Ok(folders) = std::fs::read_dir(self.claude_home.join("projects")) else {
            return vec![];
        };
        let mut files = Vec::new();
        for folder in folders.flatten() {
            let name = folder.file_name().to_string_lossy().into_owned();
            // The folder is the cwd with odd characters as '-', so a cwd
            // inside a root starts like the root's folder.
            if !roots
                .iter()
                .any(|r| r.prefixes.iter().any(|p| name.starts_with(p.as_str())))
            {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(folder.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "jsonl")
                    && entry.file_type().is_ok_and(|t| t.is_file())
                {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    fn check(
        &mut self,
        svc: &Service,
        roots: &[Root],
        path: &Path,
        meta: &Metadata,
        buffered: &HashSet<String>,
        budget: &mut Budget,
    ) -> io::Result<(Option<Update>, bool)> {
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        if !self.files.contains_key(path) {
            let Some(session) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                return Ok((None, false));
            };
            // The same session in two folders: keep the first.
            if self.files.values().any(|f| f.session == session) {
                return Ok((None, false));
            }
            self.files.insert(
                path.to_path_buf(),
                FileState {
                    session,
                    kind: Kind::Unknown,
                    head_len: 0,
                    ino: meta.ino(),
                    offset: 0,
                    skipping: false,
                    title: None,
                    started_at: None,
                    mtime,
                    announced: false,
                },
            );
        }
        let worktrees = self.worktrees.clone();
        let file = self.files.get_mut(path).expect("just added");
        file.mtime = mtime;

        // The head, for the cwd and title, until it has them or is full.
        let want_head = match file.kind {
            Kind::Unknown => true,
            Kind::Known { .. } => file.title.is_none() && file.head_len < HEAD_BYTES,
            Kind::Skip => false,
        };
        if want_head && len != file.head_len {
            let size = len.min(HEAD_BYTES);
            if budget.left < size {
                return Ok((None, true));
            }
            budget.spend(size);
            let was_unknown = matches!(file.kind, Kind::Unknown);
            read_head(path, file, len, size)?;
            if was_unknown
                && matches!(file.kind, Kind::Known { .. })
                && svc.is_crew_session(&file.session)
            {
                file.kind = Kind::Skip;
            }
            if matches!(file.kind, Kind::Unknown) && len >= HEAD_BYTES {
                // A full head without a cwd: not a session we can place.
                file.kind = Kind::Skip;
            }
            file.started_at.get_or_insert(millis(mtime));
        }
        let Kind::Known { cwd, real } = &file.kind else {
            return Ok((None, false));
        };
        let Some(project) = project_of(roots, &worktrees, real) else {
            return Ok((None, false));
        };
        let cwd = cwd.clone();

        let mut items = Vec::new();
        let mut reset = None;
        let mut more = false;
        if buffered.contains(&file.session) {
            let rotated = meta.ino() != file.ino || len < file.offset;
            if rotated {
                // A whole read, so a short budget can't pass for an
                // over-long line.
                if budget.left < MAX_READ {
                    more = true;
                } else {
                    let mut f = File::open(path)?;
                    let tail = tail_range(&mut f, len, TAIL_LINES, budget.left)?;
                    budget.spend(tail.scanned);
                    reset = Some(take_lines(file, &tail.bytes));
                    file.ino = meta.ino();
                    file.offset = tail.end;
                    file.skipping = tail.over_long;
                }
            } else if len > file.offset {
                let available = len - file.offset;
                let size = available.min(MAX_READ).min(budget.left);
                if size == 0 {
                    more = true;
                } else {
                    let mut f = File::open(path)?;
                    let bytes = read_range(&mut f, file.offset, file.offset + size)?;
                    budget.spend(size);
                    let split = complete_lines(&bytes, file.skipping, MAX_READ);
                    items = take_lines(file, &bytes[split.lines]);
                    file.offset += split.used as u64;
                    file.skipping = split.skipping;
                    more = size < available;
                }
            }
        }

        let mut info = TerminalSession {
            id: file.session.clone(),
            project,
            cwd,
            title: file.title.clone().unwrap_or_default(),
            started_at: file.started_at.unwrap_or_else(|| millis(mtime)),
            last_active_at: millis(mtime),
            live: false,
        };
        let announce = !file.announced;
        file.announced = true;
        info.live = self.is_live(&info.id, mtime);
        Ok((
            Some(Update {
                info,
                items,
                reset,
                announce,
            }),
            more,
        ))
    }

    /// Reads the backlog of a session for its first subscriber, or counts
    /// one more subscriber.
    fn load(&mut self, svc: &Service, session: &str) -> Result<(), String> {
        let missing = || format!("There is no terminal session {session}.");
        {
            let mut terminal = svc.terminal();
            let entry = terminal.sessions.get_mut(session).ok_or_else(missing)?;
            if entry.buffer.is_some() {
                entry.subscribers += 1;
                return Ok(());
            }
        }
        let (path, file) = self
            .files
            .iter_mut()
            .find(|(_, f)| f.session == session)
            .ok_or_else(missing)?;
        // Read without the lock. Only this task adds items, so none come
        // in meanwhile.
        let read = || -> io::Result<(Tail, u64)> {
            let mut f = File::open(path)?;
            let meta = f.metadata()?;
            Ok((
                tail_range(&mut f, meta.len(), TAIL_LINES, MAX_READ)?,
                meta.ino(),
            ))
        };
        let (tail, ino) = read().map_err(|e| format!("Can't read the terminal session: {e}"))?;
        let items = take_lines(file, &tail.bytes);
        file.ino = ino;
        file.offset = tail.end;
        file.skipping = tail.over_long;
        let mut terminal = svc.terminal();
        let entry = terminal.sessions.get_mut(session).ok_or_else(missing)?;
        entry.fill(items);
        entry.subscribers += 1;
        Ok(())
    }

    /// Reads `<claude home>/sessions/*.json`, one entry per running CLI.
    fn read_registry(&mut self) {
        self.registry_read = Some(Instant::now());
        let Ok(entries) = std::fs::read_dir(self.claude_home.join("sessions")) else {
            self.registry = None;
            return;
        };
        let mut registry = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(f) = File::open(&path) else { continue };
            let mut text = String::new();
            if f.take(REGISTRY_ENTRY_BYTES)
                .read_to_string(&mut text)
                .is_err()
            {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let (Some(id), Some(pid)) = (
                v.get("sessionId").and_then(|s| s.as_str()),
                v.get("pid")
                    .and_then(|p| p.as_i64())
                    .and_then(|p| i32::try_from(p).ok()),
            ) else {
                continue;
            };
            let status = v.get("status").and_then(|s| s.as_str()).map(String::from);
            registry.insert(id.to_string(), Running { pid, status });
        }
        self.registry = Some(registry);
    }

    /// Whether Claude Code works in the session now: its registry entry
    /// says `busy` and its process is there. Without a registry folder,
    /// or a status in the entry, a recent write counts.
    fn is_live(&self, session: &str, mtime: SystemTime) -> bool {
        let recent = || mtime.elapsed().is_ok_and(|age| age < RECENT) || mtime > SystemTime::now();
        let Some(registry) = &self.registry else {
            return recent();
        };
        let Some(running) = registry.get(session) else {
            return false;
        };
        if !pid_alive(running.pid) {
            return false;
        }
        match running.status.as_deref() {
            Some(status) => status == "busy",
            None => recent(),
        }
    }
}

/// Reads the first `size` bytes of the file for the session's cwd,
/// entrypoint, start time and title.
fn read_head(path: &Path, file: &mut FileState, len: u64, size: u64) -> io::Result<()> {
    file.head_len = len;
    let mut f = File::open(path)?;
    let bytes = read_range(&mut f, 0, size)?;
    let complete = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(last) => &bytes[..last + 1],
        None => return Ok(()),
    };
    let mut cwd = None;
    let mut headless = false;
    for line in complete.split(|b| *b == b'\n') {
        let parsed = transcript::parse(&String::from_utf8_lossy(line));
        if cwd.is_none() {
            cwd = parsed.info.cwd;
        }
        headless |= parsed.info.entrypoint.as_deref() == Some("sdk-cli");
        if file.started_at.is_none() {
            file.started_at = parsed.info.timestamp;
        }
        if file.title.is_none() {
            file.title = first_title(&parsed.items);
        }
    }
    if !matches!(file.kind, Kind::Unknown) {
        return Ok(());
    }
    if headless {
        file.kind = Kind::Skip;
    } else if let Some(cwd) = cwd {
        let real = real_path(Path::new(&cwd));
        file.kind = Kind::Known { cwd, real };
    }
    Ok(())
}

/// Parses complete lines, notes a title, and returns their items.
fn take_lines(file: &mut FileState, bytes: &[u8]) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let parsed = transcript::parse(&String::from_utf8_lossy(line));
        if file.title.is_none() {
            file.title = first_title(&parsed.items);
        }
        items.extend(parsed.items);
    }
    items
}

fn first_title(items: &[TranscriptItem]) -> Option<String> {
    items.iter().find_map(|i| match i {
        TranscriptItem::UserPrompt(text) => transcript::title_from(text),
        TranscriptItem::Event(_) => None,
    })
}

/// How a read splits into whole lines.
#[derive(Debug, PartialEq)]
struct Split {
    /// The whole lines, within the read.
    lines: Range<usize>,
    /// Bytes done with: skipped, or in whole lines.
    used: usize,
    /// Whether the read ends inside a line being skipped.
    skipping: bool,
}

/// Splits a read into whole lines. With `skipping`, bytes up to the first
/// line end belong to an over-long line and are dropped. A line with no
/// end within `max_line` bytes of its start is over-long. A part line at
/// the end of a shorter read waits for the rest.
fn complete_lines(bytes: &[u8], skipping: bool, max_line: u64) -> Split {
    let mut start = 0;
    if skipping {
        match bytes.iter().position(|b| *b == b'\n') {
            Some(at) => start = at + 1,
            None => {
                return Split {
                    lines: 0..0,
                    used: bytes.len(),
                    skipping: true,
                };
            }
        }
    }
    let rest = &bytes[start..];
    match rest.iter().rposition(|b| *b == b'\n') {
        Some(last) => Split {
            lines: start..start + last + 1,
            used: start + last + 1,
            skipping: false,
        },
        // Only what is left after the skipped part counts.
        None if rest.len() as u64 >= max_line => Split {
            lines: start..start,
            used: bytes.len(),
            skipping: true,
        },
        None => Split {
            lines: start..start,
            used: start,
            skipping: false,
        },
    }
}

/// The last lines of a file.
#[derive(Debug)]
pub(crate) struct Tail {
    /// The whole lines that end at `end`.
    pub(crate) bytes: Vec<u8>,
    pub(crate) end: u64,
    /// Bytes read to find them.
    pub(crate) scanned: u64,
    /// No line ends within reach: the file ends in an over-long line.
    pub(crate) over_long: bool,
}

/// The last `max_lines` whole lines in the first `len` bytes, at most
/// `max_bytes` read. Reads back from `len` in chunks, so a large file is
/// not read whole.
pub(crate) fn tail_range(
    f: &mut File,
    len: u64,
    max_lines: usize,
    max_bytes: u64,
) -> io::Result<Tail> {
    let floor = len.saturating_sub(max_bytes);
    // Chunks from the end backwards.
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut pos = len;
    let mut end = None;
    let mut lowest = None;
    let mut lines = 0;
    let mut start = None;
    'read: while pos > floor {
        let n = (pos - floor).min(CHUNK as u64) as usize;
        pos -= n as u64;
        let mut buf = vec![0u8; n];
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut buf)?;
        for i in (0..n).rev() {
            if buf[i] != b'\n' {
                continue;
            }
            let at = pos + i as u64;
            match end {
                None => end = Some(at + 1),
                Some(_) => {
                    lines += 1;
                    if lines == max_lines {
                        start = Some(at + 1);
                        chunks.push(buf);
                        break 'read;
                    }
                }
            }
            lowest = Some(at);
        }
        chunks.push(buf);
    }
    let scanned = len - pos;
    let Some(end) = end else {
        // Over long if cut by the limit; else a part line to wait for.
        let at = if floor > 0 { len } else { 0 };
        return Ok(Tail {
            bytes: vec![],
            end: at,
            scanned,
            over_long: floor > 0,
        });
    };
    let start = match start {
        Some(start) => start,
        None if floor == 0 => 0,
        // Cut by the byte limit: start at the first whole line within it.
        None => lowest.map_or(end, |at| at + 1),
    };
    let mut window = Vec::with_capacity(scanned as usize);
    for chunk in chunks.iter().rev() {
        window.extend_from_slice(chunk);
    }
    let bytes = window[(start - pos) as usize..(end - pos) as usize].to_vec();
    Ok(Tail {
        bytes,
        end,
        scanned,
        over_long: false,
    })
}

fn read_range(f: &mut File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(end.saturating_sub(start) as usize);
    f.seek(SeekFrom::Start(start))?;
    f.take(end.saturating_sub(start)).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The project a session in `cwd` belongs to: the deepest root that
/// holds it. Thread worktrees belong to none.
fn project_of(roots: &[Root], worktrees: &Path, cwd: &Path) -> Option<ProjectId> {
    if cwd.starts_with(worktrees) {
        return None;
    }
    roots
        .iter()
        .filter_map(|r| {
            r.paths
                .iter()
                .filter(|root| cwd.starts_with(root))
                .map(|root| root.components().count())
                .max()
                .map(|depth| (depth, r.id))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, id)| id)
}

/// The folder Claude Code keeps a cwd's sessions in.
fn folder_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The path with links resolved, or as it is if it is gone.
fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn millis(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn pid_alive(pid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    if pid <= 0 {
        return false;
    }
    matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::CliBins;
    use crew_core::store::Store;
    use std::io::Write;

    fn file_with(content: &[u8]) -> (tempfile::NamedTempFile, File) {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(content).unwrap();
        let f = File::open(tmp.path()).unwrap();
        (tmp, f)
    }

    /// Where the tail starts and ends.
    fn range(t: &Tail) -> (u64, u64) {
        (t.end - t.bytes.len() as u64, t.end)
    }

    #[test]
    fn the_tail_is_the_last_whole_lines() {
        let (_tmp, mut f) = file_with(b"a\nbb\nccc\npartial");
        let len = f.metadata().unwrap().len();
        let t = tail_range(&mut f, len, 2, MAX_READ).unwrap();
        assert_eq!((range(&t), t.bytes.as_slice()), ((2, 9), &b"bb\nccc\n"[..]));
        assert_eq!(
            range(&tail_range(&mut f, len, 10, MAX_READ).unwrap()),
            (0, 9)
        );
        // Up to a given end.
        assert_eq!(range(&tail_range(&mut f, 5, 10, MAX_READ).unwrap()), (0, 5));
        // Nothing whole yet.
        let t = tail_range(&mut f, 1, 10, MAX_READ).unwrap();
        assert!(t.bytes.is_empty() && !t.over_long);
    }

    #[test]
    fn the_tail_stops_at_the_byte_limit_on_a_line_start() {
        let mut content = vec![b'x'; 3 * CHUNK];
        content.extend_from_slice(b"\none\ntwo\n");
        let (_tmp, mut f) = file_with(&content);
        let len = content.len() as u64;
        let t = tail_range(&mut f, len, 100, 20).unwrap();
        assert_eq!(t.bytes, b"one\ntwo\n");
        assert!(t.scanned <= 20);
        // One line longer than the limit gives nothing.
        let t = tail_range(&mut f, 3 * CHUNK as u64 + 1, 100, 20).unwrap();
        assert!(t.bytes.is_empty());
        // No line end at all within reach: over long.
        let t = tail_range(&mut f, 3 * CHUNK as u64, 100, 20).unwrap();
        assert!(t.bytes.is_empty() && t.over_long);
    }

    #[test]
    fn tails_cross_chunks() {
        let mut content = Vec::new();
        for i in 0..50_000 {
            writeln!(content, "line {i}").unwrap();
        }
        let (_tmp, mut f) = file_with(&content);
        let len = content.len() as u64;
        let t = tail_range(&mut f, len, 3, MAX_READ).unwrap();
        assert_eq!(t.bytes, b"line 49997\nline 49998\nline 49999\n");
        assert!(t.scanned < CHUNK as u64 + 1);
        let t = tail_range(&mut f, len, 20_000, MAX_READ).unwrap();
        assert!(t.bytes.starts_with(b"line 30000\n"));
    }

    #[test]
    fn reads_split_into_whole_lines() {
        assert_eq!(
            complete_lines(b"a\nb\npart", false, 100),
            Split {
                lines: 0..4,
                used: 4,
                skipping: false
            }
        );
        // A part line in a short read waits.
        assert_eq!(
            complete_lines(b"part", false, 100),
            Split {
                lines: 0..0,
                used: 0,
                skipping: false
            }
        );
        // A line with no end in a whole read is skipped.
        assert_eq!(
            complete_lines(b"0123456789", false, 10),
            Split {
                lines: 0..0,
                used: 10,
                skipping: true
            }
        );
        // The rest of a skipped line goes, then whole lines are kept.
        assert_eq!(
            complete_lines(b"tail\nnext\n", true, 10),
            Split {
                lines: 5..10,
                used: 10,
                skipping: false
            }
        );
        // After a skipped part, a line cut by the read waits: only the
        // part after the skip counts toward the limit.
        assert_eq!(
            complete_lines(b"end\n012345", true, 10),
            Split {
                lines: 4..4,
                used: 4,
                skipping: false
            }
        );
        // Still inside the skipped line.
        assert_eq!(
            complete_lines(b"0123", true, 10),
            Split {
                lines: 0..0,
                used: 4,
                skipping: true
            }
        );
    }

    #[test]
    fn projects_match_whole_path_parts_and_skip_worktrees() {
        let a = ProjectId::new();
        let b = ProjectId::new();
        let roots = vec![
            Root {
                id: a,
                paths: vec![PathBuf::from("/work/demo")],
                prefixes: vec![],
            },
            Root {
                id: b,
                paths: vec![PathBuf::from("/work/demo/sub")],
                prefixes: vec![],
            },
        ];
        let wt = Path::new("/work/demo/.crew/worktrees");
        assert_eq!(project_of(&roots, wt, Path::new("/work/demo")), Some(a));
        assert_eq!(project_of(&roots, wt, Path::new("/work/demo/x/y")), Some(a));
        assert_eq!(
            project_of(&roots, wt, Path::new("/work/demo/sub/z")),
            Some(b)
        );
        assert_eq!(project_of(&roots, wt, Path::new("/work/demo2")), None);
        assert_eq!(
            project_of(&roots, wt, Path::new("/work/demo/.crew/worktrees/demo/t")),
            None
        );
    }

    #[test]
    fn folder_names_follow_claude_code() {
        assert_eq!(
            folder_name(Path::new("/work/demo/.config/my_app")),
            "-work-demo--config-my-app"
        );
    }

    /// A service with one project at `<tmp>/ws` and `n` sessions in it,
    /// each `size` bytes and written an hour ago.
    fn many_sessions(n: usize, size: usize) -> (tempfile::TempDir, Service, Scanner) {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(&base.join("crew.db")).unwrap();
        store.create_project("demo", &root, "main").unwrap();
        let svc = Service::new(
            store,
            4,
            CliBins::default(),
            base.join("crew/worktrees"),
            None,
            Duration::from_secs(60),
            64,
        );
        let claude = base.join("claude");
        let folder = claude.join("projects").join(folder_name(&root));
        std::fs::create_dir_all(&folder).unwrap();
        let filler = format!(
            "{}\n",
            serde_json::json!({"type": "attachment", "attachment": {"type": "x", "pad": "x".repeat(1000)}})
        );
        for i in 0..n {
            let id = format!("00000000-0000-4000-8000-{i:012}");
            let first = serde_json::json!({
                "type": "user", "message": {"content": format!("ask {i}")},
                "sessionId": id, "cwd": root, "entrypoint": "cli",
                "timestamp": "2026-11-20T10:00:00Z",
            });
            let mut text = format!("{first}\n");
            while text.len() < size {
                text.push_str(&filler);
            }
            let path = folder.join(format!("{id}.jsonl"));
            std::fs::write(&path, text).unwrap();
            let f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(3600))
                .unwrap();
        }
        let scanner = Scanner::new(claude, base.join("crew/worktrees"));
        (tmp, svc, scanner)
    }

    #[test]
    fn old_sessions_cost_only_their_heads() {
        let (_tmp, svc, mut scanner) = many_sessions(50, 1024 * 1024);
        assert!(!scanner.scan(&svc));
        assert!(
            scanner.bytes_read <= 50 * HEAD_BYTES,
            "read {} bytes",
            scanner.bytes_read
        );
        let sessions = svc.terminal().sessions();
        assert_eq!(sessions.len(), 50);
        assert!(
            sessions
                .iter()
                .all(|s| s.title.starts_with("ask ") && !s.live)
        );
        // Nothing changed: nothing read.
        let before = scanner.bytes_read;
        assert!(!scanner.scan(&svc));
        assert_eq!(scanner.bytes_read, before);
    }

    #[test]
    fn one_scan_reads_at_most_its_budget() {
        // 200 heads of 64 KB are more than one scan may read.
        let (_tmp, svc, mut scanner) = many_sessions(200, 100 * 1024);
        assert!(scanner.scan(&svc), "should leave work for the next scan");
        assert!(
            scanner.bytes_read <= MAX_READ,
            "read {}",
            scanner.bytes_read
        );
        let first = svc.terminal().sessions().len();
        assert!(first > 0 && first < 200, "{first}");
        let before = scanner.bytes_read;
        assert!(!scanner.scan(&svc));
        assert!(scanner.bytes_read - before <= MAX_READ);
        assert_eq!(svc.terminal().sessions().len(), 200);
    }
}
