//! Terminal sessions: Claude Code sessions people run in their own
//! terminals, read from the claude home and shown under their project.
//!
//! Read-only. Crew never writes under the claude home, resumes these
//! sessions or sends them input.
//!
//! A watcher task tails the session files under `<claude home>/projects`.
//! It is woken by file events, with a poll as backup, and reads the
//! `<claude home>/sessions` registry for whether a session works now. It
//! never holds the service locks while it reads files.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
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

/// Lines read from the end of a file on first sight, and items kept for
/// a subscriber's backlog.
pub const TAIL_LINES: usize = 2000;
/// Most bytes read from one file in one go.
pub const MAX_READ: u64 = 8 * 1024 * 1024;
/// Most bytes read from the start of a file for its cwd and title.
const HEAD_BYTES: u64 = 1024 * 1024;
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
    /// Seq of the next item. Counts every item read, subscribed or not.
    next_seq: u64,
    /// The last items with their seqs, kept while someone subscribes.
    buffer: Option<VecDeque<(u64, TranscriptItem)>>,
    subscribers: usize,
}

/// Requests to the watcher task.
pub(crate) enum Cmd {
    /// Projects changed: match sessions again.
    Rescan,
    /// Someone subscribes to a session: load its backlog if needed.
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
    fn apply(&self, updates: Vec<Update>) {
        if updates.is_empty() {
            return;
        }
        let mut db = self.db();
        let mut terminal = self.terminal();
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
            if u.backlog_only {
                entry.next_seq += u.items.len() as u64;
            } else {
                for item in u.items {
                    let seq = entry.next_seq;
                    entry.next_seq += 1;
                    let Some(buffer) = entry.buffer.as_mut() else {
                        continue;
                    };
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
    items: Vec<TranscriptItem>,
    /// The items were there before anyone could subscribe: count them but
    /// don't send them.
    backlog_only: bool,
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
                    // A file has more than one read's worth: go on soon.
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
    ino: u64,
    /// Next byte to read. Always at a line start.
    offset: u64,
    /// Whether the tail was read.
    tailed: bool,
    /// Inside a line too long to read: drop bytes up to its end.
    skipping: bool,
    title: Option<String>,
    started_at: Option<i64>,
    last_at: Option<i64>,
    mtime: SystemTime,
}

/// One registry entry of a running Claude Code.
struct Running {
    pid: i32,
    status: Option<String>,
}

struct Scanner {
    claude_home: PathBuf,
    worktrees: PathBuf,
    files: HashMap<PathBuf, FileState>,
    /// By session id. `None` if there is no registry folder.
    registry: Option<HashMap<String, Running>>,
    registry_read: Option<Instant>,
}

/// A project root as given and as resolved.
struct Root {
    id: ProjectId,
    paths: Vec<PathBuf>,
    prefixes: Vec<String>,
}

impl Scanner {
    fn new(claude_home: PathBuf, worktrees: PathBuf) -> Scanner {
        Scanner {
            claude_home,
            worktrees: real_path(&worktrees),
            files: HashMap::new(),
            registry: None,
            registry_read: None,
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

    /// Reads what changed and tells clients. Returns whether a file has
    /// more to read.
    fn scan(&mut self, svc: &Service) -> bool {
        if self
            .registry_read
            .is_none_or(|at| at.elapsed() >= SLOW_POLL)
        {
            self.read_registry();
        }
        let roots = Self::roots(svc);
        let mut updates = Vec::new();
        let mut more = false;
        if !roots.is_empty() {
            for path in self.session_files(&roots) {
                match self.check(svc, &roots, &path) {
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
        }
        svc.apply(updates);
        more
    }

    /// Session files in folders that may belong to a project.
    fn session_files(&self, roots: &[Root]) -> Vec<PathBuf> {
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
    ) -> io::Result<(Option<Update>, bool)> {
        let meta = std::fs::metadata(path)?;
        let len = meta.len();
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
                    tailed: false,
                    skipping: false,
                    title: None,
                    started_at: None,
                    last_at: None,
                    mtime: meta.modified().unwrap_or(UNIX_EPOCH),
                },
            );
        }
        let worktrees = self.worktrees.clone();
        let file = self.files.get_mut(path).expect("just added");
        file.mtime = meta.modified().unwrap_or(file.mtime);

        if matches!(file.kind, Kind::Unknown) && len != file.head_len {
            read_head(path, file, len)?;
            if let Kind::Known { .. } = file.kind
                && svc.is_crew_session(&file.session)
            {
                file.kind = Kind::Skip;
            }
            if matches!(file.kind, Kind::Unknown) && len >= HEAD_BYTES {
                // A megabyte without a cwd: not a session we can place.
                file.kind = Kind::Skip;
            }
        }
        let Kind::Known { cwd, real } = &file.kind else {
            return Ok((None, false));
        };
        let Some(project) = project_of(roots, &worktrees, real) else {
            return Ok((None, false));
        };
        let cwd = cwd.clone();

        let mut items = Vec::new();
        let mut backlog_only = false;
        let mut announce = false;
        let mut more = false;
        let rotated = meta.ino() != file.ino || len < file.offset;
        if !file.tailed || rotated {
            // First sight, or a new file in its place: the last lines only.
            backlog_only = !file.tailed;
            announce = !file.tailed;
            file.ino = meta.ino();
            let mut f = File::open(path)?;
            let (start, end) = tail_range(&mut f, len, TAIL_LINES, MAX_READ)?;
            let bytes = read_range(&mut f, start, end)?;
            items = take_lines(file, &bytes);
            file.offset = end;
            // No line end within reach: an over-long line.
            file.skipping = start == end && len - end >= MAX_READ;
            if file.skipping {
                file.offset = len;
            }
            file.tailed = true;
        } else if len > file.offset {
            let mut f = File::open(path)?;
            let end = len.min(file.offset + MAX_READ);
            let bytes = read_range(&mut f, file.offset, end)?;
            let mut used = 0;
            if file.skipping {
                match bytes.iter().position(|b| *b == b'\n') {
                    Some(at) => {
                        used = at + 1;
                        file.skipping = false;
                    }
                    None => used = bytes.len(),
                }
            }
            match bytes[used..].iter().rposition(|b| *b == b'\n') {
                Some(last) => {
                    let lines = &bytes[used..used + last + 1];
                    items = take_lines(file, lines);
                    used += last + 1;
                }
                None if bytes.len() as u64 == MAX_READ && !file.skipping => {
                    // One line longer than a whole read: skip it.
                    used = bytes.len();
                    file.skipping = true;
                }
                None => {}
            }
            file.offset += used as u64;
            // Cut at the read limit: go on soon.
            more = end < len;
        }

        let mtime_ms = millis(file.mtime);
        let started_at = file.started_at.unwrap_or(mtime_ms);
        let mut info = TerminalSession {
            id: file.session.clone(),
            project,
            cwd,
            title: file.title.clone().unwrap_or_default(),
            started_at,
            last_active_at: file.last_at.unwrap_or(mtime_ms).max(started_at),
            live: false,
        };
        let mtime = file.mtime;
        info.live = self.is_live(&info.id, mtime);
        Ok((
            Some(Update {
                info,
                items,
                backlog_only,
                announce,
            }),
            more,
        ))
    }

    /// Reads the backlog of a session for its first subscriber, or counts
    /// one more subscriber.
    fn load(&mut self, svc: &Service, session: &str) -> Result<(), String> {
        let missing = || format!("There is no terminal session {session}.");
        let next_seq = {
            let mut terminal = svc.terminal();
            let entry = terminal.sessions.get_mut(session).ok_or_else(missing)?;
            if entry.buffer.is_some() {
                entry.subscribers += 1;
                return Ok(());
            }
            entry.next_seq
        };
        let (path, offset) = self
            .files
            .iter()
            .find(|(_, f)| f.session == session)
            .map(|(p, f)| (p.clone(), f.offset))
            .ok_or_else(missing)?;
        // Read without the lock. Only this task adds items, so none come
        // in meanwhile.
        let items = read_tail_items(&path, offset)
            .map_err(|e| format!("Can't read the terminal session: {e}"))?;
        let keep = items.len().min(TAIL_LINES).min(next_seq as usize);
        let first = next_seq - keep as u64;
        let buffer: VecDeque<(u64, TranscriptItem)> = items[items.len() - keep..]
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, item)| (first + i as u64, item))
            .collect();
        let mut terminal = svc.terminal();
        let entry = terminal.sessions.get_mut(session).ok_or_else(missing)?;
        entry.buffer = Some(buffer);
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

/// Reads up to 1 MB from the start of the file for the session's cwd,
/// entrypoint, start time and title.
fn read_head(path: &Path, file: &mut FileState, len: u64) -> io::Result<()> {
    file.head_len = len;
    let mut f = File::open(path)?;
    let end = len.min(HEAD_BYTES);
    let bytes = read_range(&mut f, 0, end)?;
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
    if headless {
        file.kind = Kind::Skip;
    } else if let Some(cwd) = cwd {
        let real = real_path(Path::new(&cwd));
        file.kind = Kind::Known { cwd, real };
    }
    Ok(())
}

/// Parses complete lines, notes their times and title, and returns their
/// items.
fn take_lines(file: &mut FileState, bytes: &[u8]) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let parsed = transcript::parse(&String::from_utf8_lossy(line));
        if let Some(at) = parsed.info.timestamp {
            file.started_at.get_or_insert(at);
            file.last_at = Some(file.last_at.map_or(at, |last| last.max(at)));
        }
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

/// The items of the last lines before `end`.
fn read_tail_items(path: &Path, end: u64) -> io::Result<Vec<TranscriptItem>> {
    let mut f = File::open(path)?;
    let (start, end) = tail_range(&mut f, end, TAIL_LINES, MAX_READ)?;
    let bytes = read_range(&mut f, start, end)?;
    let mut items = Vec::new();
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        items.extend(transcript::parse_line(&String::from_utf8_lossy(line)));
    }
    Ok(items)
}

/// The byte range of the last `max_lines` complete lines in the first
/// `len` bytes, at most `max_bytes` long. Reads back from `len` in chunks,
/// so a large file is not read whole. The range is empty if no line ends
/// within reach.
pub(crate) fn tail_range(
    f: &mut File,
    len: u64,
    max_lines: usize,
    max_bytes: u64,
) -> io::Result<(u64, u64)> {
    let floor = len.saturating_sub(max_bytes);
    let mut buf = vec![0u8; CHUNK];
    let mut pos = len;
    let mut end = None;
    let mut lowest = None;
    let mut lines = 0;
    while pos > floor {
        let n = (pos - floor).min(CHUNK as u64) as usize;
        pos -= n as u64;
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut buf[..n])?;
        for i in (0..n).rev() {
            if buf[i] != b'\n' {
                continue;
            }
            let at = pos + i as u64;
            match end {
                None => end = Some(at + 1),
                Some(end) => {
                    lines += 1;
                    if lines == max_lines {
                        return Ok((at + 1, end));
                    }
                }
            }
            lowest = Some(at);
        }
    }
    let Some(end) = end else {
        return Ok((floor, floor));
    };
    if floor == 0 {
        return Ok((0, end));
    }
    // Cut by the byte limit: start at the first whole line within it.
    let start = lowest.map_or(end, |at| at + 1);
    Ok((start, end))
}

fn read_range(f: &mut File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0u8; end.saturating_sub(start) as usize];
    f.seek(SeekFrom::Start(start))?;
    f.read_exact(&mut bytes)?;
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
    use std::io::Write;

    fn file_with(content: &[u8]) -> (tempfile::NamedTempFile, File) {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(content).unwrap();
        let f = File::open(tmp.path()).unwrap();
        (tmp, f)
    }

    #[test]
    fn the_tail_is_the_last_whole_lines() {
        let (_tmp, mut f) = file_with(b"a\nbb\nccc\npartial");
        let len = f.metadata().unwrap().len();
        assert_eq!(tail_range(&mut f, len, 2, MAX_READ).unwrap(), (2, 9));
        assert_eq!(tail_range(&mut f, len, 10, MAX_READ).unwrap(), (0, 9));
        // Up to a given end.
        assert_eq!(tail_range(&mut f, 5, 10, MAX_READ).unwrap(), (0, 5));
        // Nothing complete yet.
        assert_eq!(tail_range(&mut f, 1, 10, MAX_READ).unwrap(), (0, 0));
    }

    #[test]
    fn the_tail_stops_at_the_byte_limit_on_a_line_start() {
        let mut content = vec![b'x'; 3 * CHUNK];
        content.extend_from_slice(b"\none\ntwo\n");
        let (_tmp, mut f) = file_with(&content);
        let len = content.len() as u64;
        let (start, end) = tail_range(&mut f, len, 100, 20).unwrap();
        assert_eq!(&content[start as usize..end as usize], b"one\ntwo\n");
        // One line longer than the limit gives nothing.
        let (start, end) = tail_range(&mut f, 3 * CHUNK as u64 + 1, 100, 20).unwrap();
        assert_eq!(start, end);
    }

    #[test]
    fn tails_cross_chunks() {
        let mut content = Vec::new();
        for i in 0..50_000 {
            writeln!(content, "line {i}").unwrap();
        }
        let (_tmp, mut f) = file_with(&content);
        let len = content.len() as u64;
        let (start, end) = tail_range(&mut f, len, 3, MAX_READ).unwrap();
        assert_eq!(
            &content[start as usize..end as usize],
            b"line 49997\nline 49998\nline 49999\n"
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
}
