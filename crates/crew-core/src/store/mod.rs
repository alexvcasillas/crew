//! SQLite persistence for projects, channels, agents, threads, messages,
//! runs and approvals.

mod approval;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::AgentSpec;
use crate::event::{Event, RunStatus};
use crate::ids::{AgentId, ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crate::policy::HandoffPolicy;

pub use approval::{ApprovalKind, ApprovalStatus, StoredApproval};

const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 2;
/// `MIGRATIONS[n]` moves a database from version `n + 1` to `n + 2`.
const MIGRATIONS: &[&str] = &[include_str!("migrate_v2.sql")];

/// Errors from the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("an agent named {0:?} already exists")]
    DuplicateAgentName(String),
    #[error("This database was written by a newer Crew.")]
    NewerSchema,
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Who wrote a message. On the wire: `{"kind":"you"}`,
/// `{"kind":"agent","id":"<uuid>"}` or `{"kind":"system"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Author {
    You,
    Agent(AgentId),
    System,
}

/// A message as saved in a thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: MessageId,
    pub thread: ThreadId,
    pub author: Author,
    pub body: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// The run that produced this message, for agent replies and system notes.
    #[serde(default)]
    pub run: Option<RunId>,
}

/// A project as saved in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredProject {
    pub id: ProjectId,
    pub name: String,
    pub root: PathBuf,
    pub main_branch: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Shell command run in a new worktree before its first run.
    pub setup_command: Option<String>,
    /// Files copied from the root into a new worktree, relative to the root.
    pub copy_files: Vec<String>,
}

/// A channel as saved in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredChannel {
    pub id: ChannelId,
    pub project: ProjectId,
    pub name: String,
    /// Unix milliseconds.
    pub created_at: i64,
    pub handoff_policy: Option<HandoffPolicy>,
    pub handoff_budget: Option<u32>,
}

/// The git worktree a thread works in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// Sha of the main branch when the worktree was made.
    pub base: String,
}

/// A thread as saved in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredThread {
    pub id: ThreadId,
    pub channel: ChannelId,
    pub title: String,
    /// Unix milliseconds.
    pub created_at: i64,
    pub worktree: Option<ThreadWorktree>,
    pub handoff_policy: Option<HandoffPolicy>,
    pub handoff_budget: Option<u32>,
    /// Agent-initiated handoffs that ran in this thread.
    pub handoffs_used: u32,
}

/// A run as saved in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRun {
    pub id: RunId,
    pub thread: ThreadId,
    pub agent: AgentId,
    pub session_id: String,
    pub status: RunStatus,
    /// Unix milliseconds.
    pub started_at: i64,
    /// Unix milliseconds, set once the run reaches a terminal status.
    pub ended_at: Option<i64>,
}

/// Narrows `Store::runs`. Unset fields match everything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunFilter {
    pub status: Option<RunStatus>,
    pub thread: Option<ThreadId>,
}

/// The crew database.
pub struct Store {
    conn: Connection,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The snake_case name of a unit enum variant.
fn enum_text<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        _ => unreachable!("unit enums serialize to a string"),
    }
}

fn parse_enum<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_string(),
    ))?)
}

fn status_text(s: &RunStatus) -> Result<String> {
    enum_text(s)
}

fn parse_status(s: &str) -> Result<RunStatus> {
    parse_enum(s)
}

fn policy_text(p: Option<HandoffPolicy>) -> Result<Option<String>> {
    p.as_ref().map(enum_text).transpose()
}

fn parse_policy(s: Option<String>) -> Result<Option<HandoffPolicy>> {
    s.as_deref().map(parse_enum).transpose()
}

fn is_terminal(s: &RunStatus) -> bool {
    matches!(
        s,
        RunStatus::Done | RunStatus::Failed | RunStatus::Stopped | RunStatus::Interrupted
    )
}

fn uuid_col(row: &Row, idx: usize) -> rusqlite::Result<Uuid> {
    let text: String = row.get(idx)?;
    Uuid::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

impl Store {
    /// Opens (creating if needed) the database file.
    pub fn open(path: &Path) -> Result<Store> {
        Self::init(Connection::open(path)?)
    }

    /// Opens a private in-memory database.
    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema);
        }
        if version == 0 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        } else if version < SCHEMA_VERSION {
            let tx = conn.unchecked_transaction()?;
            for migration in &MIGRATIONS[version as usize - 1..] {
                tx.execute_batch(migration)?;
            }
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        Ok(Store { conn })
    }

    pub fn create_project(&self, name: &str, root: &Path, main_branch: &str) -> Result<ProjectId> {
        let id = ProjectId::new();
        self.conn.execute(
            "INSERT INTO projects (id, name, root, main_branch, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.to_string(),
                name,
                root.to_string_lossy(),
                main_branch,
                now_ms()
            ],
        )?;
        Ok(id)
    }

    /// Sets what a new worktree of this project gets before its first run.
    pub fn set_project_setup(
        &self,
        p: ProjectId,
        setup_command: Option<&str>,
        copy_files: &[String],
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE projects SET setup_command = ?2, copy_files = ?3 WHERE id = ?1",
            params![
                p.to_string(),
                setup_command,
                serde_json::to_string(copy_files)?
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("project"));
        }
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<StoredProject>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{PROJECT_COLUMNS} ORDER BY p.created_at, p.rowid"))?;
        let rows = stmt.query_map([], read_project)?;
        rows.map(|r| r?).collect()
    }

    /// The project a thread belongs to.
    pub fn project_of_thread(&self, t: ThreadId) -> Result<StoredProject> {
        let row = self
            .conn
            .query_row(
                &format!(
                    "{PROJECT_COLUMNS}
                     JOIN channels c ON c.project_id = p.id
                     JOIN threads t ON t.channel_id = c.id
                     WHERE t.id = ?1"
                ),
                [t.to_string()],
                read_project,
            )
            .optional()?;
        row.ok_or(StoreError::NotFound("thread"))?
    }

    pub fn create_channel(&self, p: ProjectId, name: &str) -> Result<ChannelId> {
        let id = ChannelId::new();
        self.conn.execute(
            "INSERT INTO channels (id, project_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id.to_string(), p.to_string(), name, now_ms()],
        )?;
        Ok(id)
    }

    pub fn channels(&self, p: ProjectId) -> Result<Vec<StoredChannel>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, created_at, handoff_policy, handoff_budget
             FROM channels WHERE project_id = ?1 ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([p.to_string()], |r| {
            let channel = StoredChannel {
                id: ChannelId(uuid_col(r, 0)?),
                project: p,
                name: r.get(1)?,
                created_at: r.get(2)?,
                handoff_policy: None,
                handoff_budget: r.get(4)?,
            };
            let policy: Option<String> = r.get(3)?;
            Ok(parse_policy(policy).map(|handoff_policy| StoredChannel {
                handoff_policy,
                ..channel
            }))
        })?;
        rows.map(|r| r?).collect()
    }

    pub fn create_agent(&self, spec: &AgentSpec) -> Result<AgentId> {
        let id = AgentId::new();
        let result = self.conn.execute(
            "INSERT INTO agents (id, name, spec, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                id.to_string(),
                spec.name,
                serde_json::to_string(spec)?,
                now_ms()
            ],
        );
        match result {
            Ok(_) => Ok(id),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation
                    && e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                Err(StoreError::DuplicateAgentName(spec.name.clone()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn agents(&self) -> Result<Vec<(AgentId, AgentSpec)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, spec FROM agents ORDER BY created_at, rowid")?;
        let rows = stmt.query_map([], |r| {
            Ok((AgentId(uuid_col(r, 0)?), r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, spec) = row?;
            out.push((id, serde_json::from_str(&spec)?));
        }
        Ok(out)
    }

    pub fn create_thread(&self, c: ChannelId, title: &str) -> Result<ThreadId> {
        let id = ThreadId::new();
        self.conn.execute(
            "INSERT INTO threads (id, channel_id, title, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id.to_string(), c.to_string(), title, now_ms()],
        )?;
        Ok(id)
    }

    pub fn thread(&self, t: ThreadId) -> Result<StoredThread> {
        let row = self
            .conn
            .query_row(
                &format!("{THREAD_COLUMNS} WHERE id = ?1"),
                [t.to_string()],
                read_thread,
            )
            .optional()?;
        row.ok_or(StoreError::NotFound("thread"))?
    }

    pub fn threads(&self, c: ChannelId) -> Result<Vec<StoredThread>> {
        let mut stmt = self.conn.prepare(&format!(
            "{THREAD_COLUMNS} WHERE channel_id = ?1 ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map([c.to_string()], read_thread)?;
        rows.map(|r| r?).collect()
    }

    /// Sets or clears the thread's worktree.
    pub fn set_thread_worktree(&self, t: ThreadId, wt: Option<&ThreadWorktree>) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE threads SET worktree_path = ?2, worktree_branch = ?3, worktree_base = ?4
             WHERE id = ?1",
            params![
                t.to_string(),
                wt.map(|w| w.path.to_string_lossy().into_owned()),
                wt.map(|w| &w.branch),
                wt.map(|w| &w.base)
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("thread"));
        }
        Ok(())
    }

    /// Sets the thread's own handoff policy and budget. `None` falls back
    /// to the agent or channel.
    pub fn set_thread_handoff(
        &self,
        t: ThreadId,
        policy: Option<HandoffPolicy>,
        budget: Option<u32>,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE threads SET handoff_policy = ?2, handoff_budget = ?3 WHERE id = ?1",
            params![t.to_string(), policy_text(policy)?, budget],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("thread"));
        }
        Ok(())
    }

    /// Counts one more handoff in the thread and returns the new total.
    pub fn bump_handoffs_used(&self, t: ThreadId) -> Result<u32> {
        let used: Option<u32> = self
            .conn
            .query_row(
                "UPDATE threads SET handoffs_used = handoffs_used + 1 WHERE id = ?1
                 RETURNING handoffs_used",
                [t.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        used.ok_or(StoreError::NotFound("thread"))
    }

    /// Working directory for a thread: its worktree if it has one, else
    /// the project root.
    pub fn thread_cwd(&self, t: ThreadId) -> Result<PathBuf> {
        let root: Option<String> = self
            .conn
            .query_row(
                "SELECT COALESCE(t.worktree_path, p.root) FROM threads t
                 JOIN channels c ON c.id = t.channel_id
                 JOIN projects p ON p.id = c.project_id
                 WHERE t.id = ?1",
                [t.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        root.map(PathBuf::from)
            .ok_or(StoreError::NotFound("thread"))
    }

    /// Saves a message. `run` is the run that produced it, if any.
    pub fn add_message(
        &self,
        t: ThreadId,
        author: Author,
        body: &str,
        run: Option<RunId>,
    ) -> Result<MessageId> {
        let id = MessageId::new();
        let (kind, agent) = match author {
            Author::You => ("you", None),
            Author::Agent(a) => ("agent", Some(a.to_string())),
            Author::System => ("system", None),
        };
        self.conn.execute(
            "INSERT INTO messages (id, thread_id, author_kind, author_agent, body, created_at, run_id, ord)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                     (SELECT COALESCE(MAX(ord) + 1, 0) FROM messages WHERE thread_id = ?2))",
            params![
                id.to_string(),
                t.to_string(),
                kind,
                agent,
                body,
                now_ms(),
                run.map(|r| r.to_string())
            ],
        )?;
        Ok(id)
    }

    pub fn messages(&self, t: ThreadId) -> Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, author_kind, author_agent, body, created_at, run_id
             FROM messages WHERE thread_id = ?1 ORDER BY ord",
        )?;
        let rows = stmt.query_map([t.to_string()], |r| {
            let id = MessageId(uuid_col(r, 0)?);
            let kind: String = r.get(1)?;
            let author = match kind.as_str() {
                "you" => Author::You,
                "system" => Author::System,
                _ => Author::Agent(AgentId(uuid_col(r, 2)?)),
            };
            Ok(StoredMessage {
                id,
                thread: t,
                author,
                body: r.get(3)?,
                created_at: r.get(4)?,
                run: r
                    .get::<_, Option<String>>(5)?
                    .map(|_| uuid_col(r, 5).map(RunId))
                    .transpose()?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds a run in the `Queued` state, with no session yet.
    pub fn create_run(&self, t: ThreadId, a: AgentId) -> Result<RunId> {
        let id = RunId::new();
        self.conn.execute(
            "INSERT INTO runs (id, thread_id, agent_id, session_id, status, started_at, ord)
             VALUES (?1, ?2, ?3, '', ?4, ?5, (SELECT COALESCE(MAX(ord) + 1, 0) FROM runs))",
            params![
                id.to_string(),
                t.to_string(),
                a.to_string(),
                status_text(&RunStatus::Queued)?,
                now_ms()
            ],
        )?;
        Ok(id)
    }

    /// Moves a run to `Working` with its session id. `started_at` becomes now.
    pub fn start_run(&self, r: RunId, session_id: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE runs SET session_id = ?2, status = ?3, started_at = ?4 WHERE id = ?1",
            params![
                r.to_string(),
                session_id,
                status_text(&RunStatus::Working)?,
                now_ms()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("run"));
        }
        Ok(())
    }

    /// Sets the status. A terminal status also sets `ended_at`.
    pub fn set_run_status(&self, r: RunId, s: RunStatus) -> Result<()> {
        let ended = is_terminal(&s).then(now_ms);
        let changed = self.conn.execute(
            "UPDATE runs SET status = ?2, ended_at = ?3 WHERE id = ?1",
            params![r.to_string(), status_text(&s)?, ended],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("run"));
        }
        Ok(())
    }

    pub fn run(&self, r: RunId) -> Result<StoredRun> {
        let row = self
            .conn
            .query_row(
                &format!("{RUN_COLUMNS} WHERE id = ?1"),
                [r.to_string()],
                read_run,
            )
            .optional()?;
        row.ok_or(StoreError::NotFound("run"))?
    }

    pub fn runs(&self, filter: RunFilter) -> Result<Vec<StoredRun>> {
        let status = filter.status.as_ref().map(status_text).transpose()?;
        let thread = filter.thread.map(|t| t.to_string());
        let mut stmt = self.conn.prepare(&format!(
            "{RUN_COLUMNS} WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR thread_id = ?2) ORDER BY ord"
        ))?;
        let rows = stmt.query_map(params![status, thread], read_run)?;
        rows.map(|r| r?).collect()
    }

    /// Appends an event and returns its sequence number (0 for the first of a run).
    pub fn append_event(&self, r: RunId, e: &Event) -> Result<u64> {
        let seq: i64 = self.conn.query_row(
            "INSERT INTO events (run_id, seq, body)
             VALUES (?1, (SELECT COALESCE(MAX(seq) + 1, 0) FROM events WHERE run_id = ?1), ?2)
             RETURNING seq",
            params![r.to_string(), serde_json::to_string(e)?],
            |row| row.get(0),
        )?;
        Ok(seq as u64)
    }

    /// Events of a run with `seq >= from_seq`, in order.
    pub fn events(&self, r: RunId, from_seq: u64) -> Result<Vec<(u64, Event)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, body FROM events WHERE run_id = ?1 AND seq >= ?2 ORDER BY seq")?;
        let rows = stmt.query_map(params![r.to_string(), from_seq as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, body) = row?;
            out.push((seq as u64, serde_json::from_str(&body)?));
        }
        Ok(out)
    }

    /// Marks every `Queued` or `Working` run as `Interrupted`. Returns how
    /// many changed.
    pub fn mark_unfinished_runs_interrupted(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE runs SET status = ?1, ended_at = ?2 WHERE status IN (?3, ?4)",
            params![
                status_text(&RunStatus::Interrupted)?,
                now_ms(),
                status_text(&RunStatus::Queued)?,
                status_text(&RunStatus::Working)?
            ],
        )?)
    }
}

const PROJECT_COLUMNS: &str = "SELECT p.id, p.name, p.root, p.main_branch, p.created_at,
     p.setup_command, p.copy_files FROM projects p";

fn read_project(r: &Row) -> rusqlite::Result<Result<StoredProject>> {
    let copy_files: String = r.get(6)?;
    let project = StoredProject {
        id: ProjectId(uuid_col(r, 0)?),
        name: r.get(1)?,
        root: PathBuf::from(r.get::<_, String>(2)?),
        main_branch: r.get(3)?,
        created_at: r.get(4)?,
        setup_command: r.get(5)?,
        copy_files: Vec::new(),
    };
    Ok(serde_json::from_str(&copy_files)
        .map(|copy_files| StoredProject {
            copy_files,
            ..project
        })
        .map_err(StoreError::from))
}

const THREAD_COLUMNS: &str = "SELECT id, channel_id, title, created_at, worktree_path,
     worktree_branch, worktree_base, handoff_policy, handoff_budget, handoffs_used FROM threads";

fn read_thread(r: &Row) -> rusqlite::Result<Result<StoredThread>> {
    let path: Option<String> = r.get(4)?;
    let branch: Option<String> = r.get(5)?;
    let base: Option<String> = r.get(6)?;
    let worktree = match (path, branch, base) {
        (Some(path), Some(branch), Some(base)) => Some(ThreadWorktree {
            path: PathBuf::from(path),
            branch,
            base,
        }),
        _ => None,
    };
    let thread = StoredThread {
        id: ThreadId(uuid_col(r, 0)?),
        channel: ChannelId(uuid_col(r, 1)?),
        title: r.get(2)?,
        created_at: r.get(3)?,
        worktree,
        handoff_policy: None,
        handoff_budget: r.get(8)?,
        handoffs_used: r.get(9)?,
    };
    let policy: Option<String> = r.get(7)?;
    Ok(parse_policy(policy).map(|handoff_policy| StoredThread {
        handoff_policy,
        ..thread
    }))
}

const RUN_COLUMNS: &str =
    "SELECT id, thread_id, agent_id, session_id, status, started_at, ended_at FROM runs";

fn read_run(r: &Row) -> rusqlite::Result<Result<StoredRun>> {
    let status: String = r.get(4)?;
    let run = StoredRun {
        id: RunId(uuid_col(r, 0)?),
        thread: ThreadId(uuid_col(r, 1)?),
        agent: AgentId(uuid_col(r, 2)?),
        session_id: r.get(3)?,
        status: RunStatus::Queued,
        started_at: r.get(5)?,
        ended_at: r.get(6)?,
    };
    Ok(parse_status(&status).map(|status| StoredRun { status, ..run }))
}
