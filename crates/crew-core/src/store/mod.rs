//! SQLite persistence for projects, channels, agents, threads, messages,
//! runs and approvals.

mod approval;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::{AgentSpec, Runtime};
use crate::event::{Event, RunStatus};
use crate::ids::{AgentId, ApprovalId, ChannelId, MessageId, ProjectId, RunId, ThreadId};
use crate::policy::HandoffPolicy;

pub use approval::{
    ApprovalKind, ApprovalStatus, OUTSIDE_SANDBOX, StoredApproval, blocked_text, handoff_ask_text,
    paused_text, permission_text, short_ask, short_input,
};

const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 9;
/// `MIGRATIONS[n]` moves a database from version `n + 1` to `n + 2`.
const MIGRATIONS: &[&str] = &[
    include_str!("migrate_v2.sql"),
    include_str!("migrate_v3.sql"),
    include_str!("migrate_v4.sql"),
    include_str!("migrate_v5.sql"),
    include_str!("migrate_v6.sql"),
    include_str!("migrate_v7.sql"),
    include_str!("migrate_v8.sql"),
    include_str!("migrate_v9.sql"),
];

/// The global limits you set in the app. Stored as rows of `settings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Agents that may run at once, 1 to 8.
    pub max_runs: u32,
    /// While set, new runs stay queued. Live runs are not touched.
    pub paused: bool,
    /// The handoff policy when nothing narrower sets one. Only `AskFirst`
    /// and `Budget` are valid here.
    pub handoff_policy: HandoffPolicy,
    /// Handoffs per thread when nothing narrower sets a budget, 1 to 10.
    pub handoff_budget: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_runs: 4,
            paused: false,
            handoff_policy: HandoffPolicy::AskFirst,
            handoff_budget: 3,
        }
    }
}

impl Limits {
    /// Checks the ranges and policy a client may set.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(1..=8).contains(&self.max_runs) {
            return Err("Choose between 1 and 8 agents at once.".into());
        }
        if !(1..=10).contains(&self.handoff_budget) {
            return Err("Choose between 1 and 10 handoffs.".into());
        }
        if self.handoff_policy == HandoffPolicy::PerAgent {
            return Err("Choose Ask first or Run with budget.".into());
        }
        Ok(())
    }
}

/// What one runtime did in a span: runs and their summed duration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeUsage {
    pub runtime: Runtime,
    pub runs: u32,
    pub total_ms: i64,
}

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
    #[error("{0}")]
    Invalid(String),
    #[error("This database was written by a newer Crew.")]
    NewerSchema,
    #[error("Upgrading the database left broken references. It was left as it was.")]
    BrokenMigration,
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
    /// The approval a system note is about, if any.
    #[serde(default)]
    pub approval: Option<ApprovalId>,
}

/// An agent as saved in the store. Deleted agents stay, so their past
/// messages and runs can still name them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAgent {
    pub id: AgentId,
    pub spec: AgentSpec,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds, set once the agent was deleted.
    pub deleted_at: Option<i64>,
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
    /// What the channel is for. Empty if not set.
    #[serde(default)]
    pub description: String,
}

/// The git worktree a thread works in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// Sha of the main branch when the worktree was made.
    pub base: String,
    /// Set once files were copied in and the setup command succeeded.
    #[serde(default)]
    pub ready: bool,
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
    /// Unix milliseconds, set while the thread is archived.
    #[serde(default)]
    pub archived_at: Option<i64>,
}

/// What a channel overview needs from one thread's messages and asks.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadActivity {
    pub thread: ThreadId,
    /// Messages from you and from agents. System notes don't count.
    pub messages: u32,
    /// Unix milliseconds of the newest message of any kind.
    pub last_message_at: Option<i64>,
    /// The newest agent reply.
    pub latest_agent: Option<String>,
    /// Your first message.
    pub first_you: Option<String>,
    /// Whether an approval waits for a decision.
    pub needs_you: bool,
    /// The note of the newest pending approval.
    pub pending_ask: Option<String>,
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
    /// The CLI that ran it. Set when it starts; Claude Code until then.
    #[serde(default = "claude_code")]
    pub runtime: Runtime,
}

fn claude_code() -> Runtime {
    Runtime::ClaudeCode
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

fn runtime_text(r: &Runtime) -> Result<String> {
    enum_text(r)
}

fn parse_runtime(s: &str) -> Result<Runtime> {
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
        // Off while migrating: a migration may rebuild a table others point to.
        conn.pragma_update(None, "foreign_keys", false)?;
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
            if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
                return Err(StoreError::BrokenMigration);
            }
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(Store { conn })
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Saves one setting, replacing its value.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// The last context window seen for `model`, in tokens.
    pub fn context_window(&self, model: &str) -> Result<Option<u64>> {
        Ok(self
            .setting(&format!("context_window:{model}"))?
            .and_then(|raw| raw.parse().ok()))
    }

    /// Remembers the context window of `model`.
    pub fn set_context_window(&self, model: &str, tokens: u64) -> Result<()> {
        self.set_setting(&format!("context_window:{model}"), &tokens.to_string())
    }

    /// The limits. A missing or unreadable row gives its default and a
    /// number out of range is clamped, so this never fails on bad data.
    pub fn limits(&self) -> Result<Limits> {
        let d = Limits::default();
        let number = |key: &str, default: u32, range: std::ops::RangeInclusive<u32>| {
            let Some(raw) = self.setting(key)? else {
                return Ok::<u32, StoreError>(default);
            };
            Ok(match raw.parse::<u32>() {
                Ok(n) => n.clamp(*range.start(), *range.end()),
                Err(_) => {
                    eprintln!("crew: ignoring setting {key} = {raw:?}");
                    default
                }
            })
        };
        let paused = match self.setting("paused")?.as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(raw) => {
                eprintln!("crew: ignoring setting paused = {raw:?}");
                d.paused
            }
        };
        let handoff_policy = match self.setting("handoff_policy")?.as_deref() {
            None | Some("ask_first") => HandoffPolicy::AskFirst,
            Some("budget") => HandoffPolicy::Budget,
            Some(raw) => {
                eprintln!("crew: ignoring setting handoff_policy = {raw:?}");
                d.handoff_policy
            }
        };
        Ok(Limits {
            max_runs: number("max_runs", d.max_runs, 1..=8)?,
            paused,
            handoff_policy,
            handoff_budget: number("handoff_budget", d.handoff_budget, 1..=10)?,
        })
    }

    /// Saves all four limits. The caller validates them first.
    pub fn set_limits(&self, limits: &Limits) -> Result<()> {
        let policy = match limits.handoff_policy {
            HandoffPolicy::Budget => "budget",
            HandoffPolicy::AskFirst => "ask_first",
            HandoffPolicy::PerAgent => {
                return Err(StoreError::Invalid(
                    "the global handoff policy is Ask first or Run with budget".into(),
                ));
            }
        };
        let tx = self.conn.unchecked_transaction()?;
        self.set_setting("max_runs", &limits.max_runs.to_string())?;
        self.set_setting("paused", if limits.paused { "true" } else { "false" })?;
        self.set_setting("handoff_policy", policy)?;
        self.set_setting("handoff_budget", &limits.handoff_budget.to_string())?;
        tx.commit()?;
        Ok(())
    }

    /// Sets `max_runs` to `n` (clamped) unless it was saved before.
    pub fn seed_max_runs(&self, n: u32) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES ('max_runs', ?1)",
            [n.clamp(1, 8).to_string()],
        )?;
        Ok(())
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

    pub fn rename_project(&self, p: ProjectId, name: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE projects SET name = ?2 WHERE id = ?1",
            params![p.to_string(), name],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("project"));
        }
        Ok(())
    }

    /// Deletes the project with its channels, threads, messages, runs,
    /// events and approvals. Nothing on disk is touched.
    pub fn remove_project(&self, p: ProjectId) -> Result<()> {
        const THREADS: &str = "SELECT t.id FROM threads t JOIN channels c ON c.id = t.channel_id
             WHERE c.project_id = ?1";
        let tx = self.conn.unchecked_transaction()?;
        let id = p.to_string();
        for sql in [
            format!("DELETE FROM messages WHERE thread_id IN ({THREADS})"),
            format!("DELETE FROM approvals WHERE thread_id IN ({THREADS})"),
            format!(
                "DELETE FROM events WHERE run_id IN
                 (SELECT id FROM runs WHERE thread_id IN ({THREADS}))"
            ),
            format!("DELETE FROM runs WHERE thread_id IN ({THREADS})"),
            "DELETE FROM threads WHERE channel_id IN
             (SELECT id FROM channels WHERE project_id = ?1)"
                .to_string(),
            "DELETE FROM channels WHERE project_id = ?1".to_string(),
        ] {
            tx.execute(&sql, [&id])?;
        }
        let changed = tx.execute("DELETE FROM projects WHERE id = ?1", [&id])?;
        if changed == 0 {
            return Err(StoreError::NotFound("project"));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<StoredProject>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{PROJECT_COLUMNS} ORDER BY p.created_at, p.rowid"))?;
        let rows = stmt.query_map([], read_project)?;
        rows.map(|r| r?).collect()
    }

    pub fn project(&self, p: ProjectId) -> Result<StoredProject> {
        let row = self
            .conn
            .query_row(
                &format!("{PROJECT_COLUMNS} WHERE p.id = ?1"),
                [p.to_string()],
                read_project,
            )
            .optional()?;
        row.ok_or(StoreError::NotFound("project"))?
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
        let mut stmt = self.conn.prepare(&format!(
            "{CHANNEL_COLUMNS} WHERE project_id = ?1 ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map([p.to_string()], read_channel)?;
        rows.map(|r| r?).collect()
    }

    pub fn channel(&self, c: ChannelId) -> Result<StoredChannel> {
        let row = self
            .conn
            .query_row(
                &format!("{CHANNEL_COLUMNS} WHERE id = ?1"),
                [c.to_string()],
                read_channel,
            )
            .optional()?;
        row.ok_or(StoreError::NotFound("channel"))?
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
        result.map_err(|e| name_taken(e, spec))?;
        Ok(id)
    }

    /// Replaces the spec of an agent that isn't deleted.
    pub fn update_agent(&self, a: AgentId, spec: &AgentSpec) -> Result<()> {
        let changed = self
            .conn
            .execute(
                "UPDATE agents SET name = ?2, spec = ?3 WHERE id = ?1 AND deleted_at IS NULL",
                params![a.to_string(), spec.name, serde_json::to_string(spec)?],
            )
            .map_err(|e| name_taken(e, spec))?;
        if changed == 0 {
            return Err(StoreError::NotFound("agent"));
        }
        Ok(())
    }

    /// Marks an agent deleted. It keeps its messages and runs, and its
    /// name is free for a new agent.
    pub fn delete_agent(&self, a: AgentId) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE agents SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![a.to_string(), now_ms()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("agent"));
        }
        Ok(())
    }

    /// One agent, deleted or not.
    pub fn agent(&self, a: AgentId) -> Result<StoredAgent> {
        let row = self
            .conn
            .query_row(
                &format!("{AGENT_COLUMNS} WHERE id = ?1"),
                [a.to_string()],
                read_agent,
            )
            .optional()?;
        row.ok_or(StoreError::NotFound("agent"))?
    }

    /// Every agent, deleted ones included.
    pub fn all_agents(&self) -> Result<Vec<StoredAgent>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{AGENT_COLUMNS} ORDER BY created_at, rowid"))?;
        let rows = stmt.query_map([], read_agent)?;
        rows.map(|r| r?).collect()
    }

    /// Agents that aren't deleted: those you can mention.
    pub fn agents(&self) -> Result<Vec<(AgentId, AgentSpec)>> {
        Ok(self
            .all_agents()?
            .into_iter()
            .filter(|a| a.deleted_at.is_none())
            .map(|a| (a.id, a.spec))
            .collect())
    }

    /// Every agent's spec, deleted ones included, for naming past work.
    pub fn agents_with_deleted(&self) -> Result<Vec<(AgentId, AgentSpec)>> {
        Ok(self
            .all_agents()?
            .into_iter()
            .map(|a| (a.id, a.spec))
            .collect())
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

    /// Threads that aren't archived, of one project or of all of them.
    pub fn open_threads(&self, p: Option<ProjectId>) -> Result<Vec<StoredThread>> {
        let mut stmt = self.conn.prepare(&format!(
            "{THREAD_COLUMNS} WHERE archived_at IS NULL AND (?1 IS NULL OR channel_id IN
               (SELECT id FROM channels WHERE project_id = ?1))
             ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map([p.map(|p| p.to_string())], read_thread)?;
        rows.map(|r| r?).collect()
    }

    /// Every thread of the project, archived ones included.
    pub fn project_threads(&self, p: ProjectId) -> Result<Vec<StoredThread>> {
        let mut stmt = self.conn.prepare(&format!(
            "{THREAD_COLUMNS} WHERE channel_id IN (SELECT id FROM channels WHERE project_id = ?1)
             ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map([p.to_string()], read_thread)?;
        rows.map(|r| r?).collect()
    }

    /// Threads that aren't archived with a message from you or an agent
    /// holding `needle`, which must be lowercase. Matching is ASCII case
    /// insensitive.
    pub fn threads_mentioning(&self, needle: &str, p: Option<ProjectId>) -> Result<Vec<ThreadId>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT m.thread_id FROM messages m JOIN threads t ON t.id = m.thread_id
             WHERE t.archived_at IS NULL AND m.author_kind <> 'system'
               AND (?2 IS NULL OR t.channel_id IN
                 (SELECT id FROM channels WHERE project_id = ?2))
               AND instr(lower(m.body), ?1) > 0",
        )?;
        let rows = stmt.query_map(params![needle, p.map(|p| p.to_string())], |r| {
            Ok(ThreadId(uuid_col(r, 0)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn rename_thread(&self, t: ThreadId, title: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE threads SET title = ?2 WHERE id = ?1",
            params![t.to_string(), title],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("thread"));
        }
        Ok(())
    }

    /// Archives the thread now, or brings it back.
    pub fn set_thread_archived(&self, t: ThreadId, archived: bool) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE threads SET archived_at = ?2 WHERE id = ?1",
            params![t.to_string(), archived.then(now_ms)],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("thread"));
        }
        Ok(())
    }

    /// Message counts, excerpts and pending asks of every thread in the
    /// channel.
    pub fn thread_activity(&self, c: ChannelId) -> Result<Vec<ThreadActivity>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id,
               (SELECT COUNT(*) FROM messages m
                 WHERE m.thread_id = t.id AND m.author_kind <> 'system'),
               (SELECT MAX(m.created_at) FROM messages m WHERE m.thread_id = t.id),
               (SELECT m.body FROM messages m
                 WHERE m.thread_id = t.id AND m.author_kind = 'agent'
                 ORDER BY m.ord DESC LIMIT 1),
               (SELECT m.body FROM messages m
                 WHERE m.thread_id = t.id AND m.author_kind = 'you'
                 ORDER BY m.ord LIMIT 1),
               EXISTS(SELECT 1 FROM approvals a
                 WHERE a.thread_id = t.id AND a.status = ?2),
               (SELECT m.body FROM messages m JOIN approvals a ON a.id = m.approval_id
                 WHERE m.thread_id = t.id AND a.status = ?2
                 ORDER BY m.ord DESC LIMIT 1)
             FROM threads t WHERE t.channel_id = ?1 ORDER BY t.created_at, t.rowid",
        )?;
        let pending = enum_text(&ApprovalStatus::Pending)?;
        let rows = stmt.query_map(params![c.to_string(), pending], |r| {
            Ok(ThreadActivity {
                thread: ThreadId(uuid_col(r, 0)?),
                messages: r.get(1)?,
                last_message_at: r.get(2)?,
                latest_agent: r.get(3)?,
                first_you: r.get(4)?,
                needs_you: r.get(5)?,
                pending_ask: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every run of every thread in the channel, in order.
    pub fn channel_runs(&self, c: ChannelId) -> Result<Vec<StoredRun>> {
        let mut stmt = self.conn.prepare(&format!(
            "{RUN_COLUMNS} WHERE thread_id IN (SELECT id FROM threads WHERE channel_id = ?1)
             ORDER BY ord"
        ))?;
        let rows = stmt.query_map([c.to_string()], read_run)?;
        rows.map(|r| r?).collect()
    }

    /// Sets or clears the thread's worktree.
    pub fn set_thread_worktree(&self, t: ThreadId, wt: Option<&ThreadWorktree>) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE threads SET worktree_path = ?2, worktree_branch = ?3, worktree_base = ?4,
             worktree_ready = ?5 WHERE id = ?1",
            params![
                t.to_string(),
                wt.map(|w| w.path.to_string_lossy().into_owned()),
                wt.map(|w| &w.branch),
                wt.map(|w| &w.base),
                wt.is_some_and(|w| w.ready)
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("thread"));
        }
        Ok(())
    }

    /// Renames the channel and sets its description. The caller checks
    /// the name first.
    pub fn update_channel(&self, c: ChannelId, name: &str, description: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE channels SET name = ?2, description = ?3 WHERE id = ?1",
            params![c.to_string(), name, description],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("channel"));
        }
        Ok(())
    }

    /// Sets the channel's handoff policy and budget. `None` falls back to
    /// the global default.
    pub fn set_channel_handoff(
        &self,
        c: ChannelId,
        policy: Option<HandoffPolicy>,
        budget: Option<u32>,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE channels SET handoff_policy = ?2, handoff_budget = ?3 WHERE id = ?1",
            params![c.to_string(), policy_text(policy)?, budget],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("channel"));
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
        self.insert_message(t, author, body, run, None)
    }

    /// Saves a system note about `approval`, asked by run `run`.
    pub fn add_approval_message(
        &self,
        t: ThreadId,
        body: &str,
        run: RunId,
        approval: ApprovalId,
    ) -> Result<MessageId> {
        self.insert_message(t, Author::System, body, Some(run), Some(approval))
    }

    fn insert_message(
        &self,
        t: ThreadId,
        author: Author,
        body: &str,
        run: Option<RunId>,
        approval: Option<ApprovalId>,
    ) -> Result<MessageId> {
        let id = MessageId::new();
        let (kind, agent) = match author {
            Author::You => ("you", None),
            Author::Agent(a) => ("agent", Some(a.to_string())),
            Author::System => ("system", None),
        };
        self.conn.execute(
            "INSERT INTO messages (id, thread_id, author_kind, author_agent, body, created_at, run_id,
                                   approval_id, ord)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                     (SELECT COALESCE(MAX(ord) + 1, 0) FROM messages WHERE thread_id = ?2))",
            params![
                id.to_string(),
                t.to_string(),
                kind,
                agent,
                body,
                now_ms(),
                run.map(|r| r.to_string()),
                approval.map(|a| a.to_string())
            ],
        )?;
        Ok(id)
    }

    pub fn messages(&self, t: ThreadId) -> Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, author_kind, author_agent, body, created_at, run_id, approval_id
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
                approval: r
                    .get::<_, Option<String>>(6)?
                    .map(|_| uuid_col(r, 6).map(ApprovalId))
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

    /// Adds a queued run of `a` that `from` brought in with a handoff.
    pub fn create_handoff_run(&self, t: ThreadId, a: AgentId, from: RunId) -> Result<RunId> {
        let id = self.create_run(t, a)?;
        self.conn.execute(
            "UPDATE runs SET handed_from = ?2 WHERE id = ?1",
            params![id.to_string(), from.to_string()],
        )?;
        Ok(id)
    }

    /// `(from, to)` agents of each handoff run created at or after `since`
    /// (Unix ms).
    pub fn handoffs_since(&self, since: i64) -> Result<Vec<(AgentId, AgentId)>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.agent_id, r.agent_id FROM runs r JOIN runs f ON f.id = r.handed_from
             WHERE r.started_at >= ?1 ORDER BY r.ord",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok((AgentId(uuid_col(r, 0)?), AgentId(uuid_col(r, 1)?)))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every run in the project's threads, in order.
    pub fn project_runs(&self, p: ProjectId) -> Result<Vec<StoredRun>> {
        let mut stmt = self.conn.prepare(&format!(
            "{RUN_COLUMNS} WHERE thread_id IN (SELECT t.id FROM threads t
               JOIN channels c ON c.id = t.channel_id WHERE c.project_id = ?1)
             ORDER BY ord"
        ))?;
        let rows = stmt.query_map([p.to_string()], read_run)?;
        rows.map(|r| r?).collect()
    }

    /// Every run of the agent, in order.
    pub fn agent_runs(&self, a: AgentId) -> Result<Vec<StoredRun>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{RUN_COLUMNS} WHERE agent_id = ?1 ORDER BY ord"))?;
        let rows = stmt.query_map([a.to_string()], read_run)?;
        rows.map(|r| r?).collect()
    }

    /// Moves a run to `Working` with the CLI that runs it and its session
    /// id. `started_at` becomes now.
    pub fn start_run(&self, r: RunId, runtime: &Runtime, session_id: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE runs SET session_id = ?2, status = ?3, started_at = ?4, runtime = ?5 WHERE id = ?1",
            params![
                r.to_string(),
                session_id,
                status_text(&RunStatus::Working)?,
                now_ms(),
                runtime_text(runtime)?
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("run"));
        }
        Ok(())
    }

    /// Moves a run to `Working` before its CLI starts, as while its
    /// worktree is set up. `started_at` becomes now, so time spent queued
    /// doesn't count. [`Self::start_run`] sets it again once the CLI runs.
    pub fn begin_run(&self, r: RunId) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE runs SET status = ?2, started_at = ?3 WHERE id = ?1",
            params![r.to_string(), status_text(&RunStatus::Working)?, now_ms()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("run"));
        }
        Ok(())
    }

    /// Whether any run has this session id.
    pub fn has_run_session(&self, session_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id = ?1)",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// Saves a run's session id once the CLI names it, as Codex does.
    pub fn set_run_session(&self, r: RunId, session_id: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE runs SET session_id = ?2 WHERE id = ?1",
            params![r.to_string(), session_id],
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

    /// Ended runs that started (any final status), newest `ended_at` first.
    /// `before` is an inclusive `ended_at` in Unix ms, so runs that share the
    /// last end time of a page come again on the next one and none is lost;
    /// callers drop repeats by id. `limit` is clamped to 1..=200. A run
    /// stopped while still queued never started and is left out.
    pub fn run_history(&self, before: Option<i64>, limit: u32) -> Result<Vec<StoredRun>> {
        let mut stmt = self.conn.prepare(&format!(
            "{RUN_COLUMNS} WHERE ended_at IS NOT NULL AND session_id <> ''
             AND (?1 IS NULL OR ended_at <= ?1)
             ORDER BY ended_at DESC, ord DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![before, limit.clamp(1, 200)], read_run)?;
        rows.map(|r| r?).collect()
    }

    /// Runs per runtime that ended at or after `since_ms`, counted by their
    /// full duration, plus live runs counted by their time so far at `now_ms`.
    /// The runtime is the one the run used. Queued runs and runs that never
    /// started don't count. A runtime with no runs is left out.
    pub fn usage_since(&self, since_ms: i64, now_ms: i64) -> Result<Vec<RuntimeUsage>> {
        let mut stmt = self.conn.prepare(&format!(
            "{RUN_COLUMNS} WHERE status = ?1 OR (ended_at >= ?2 AND session_id <> '') ORDER BY ord"
        ))?;
        let working = status_text(&RunStatus::Working)?;
        let runs = stmt.query_map(params![working, since_ms], read_run)?;
        let mut usage: Vec<RuntimeUsage> = Vec::new();
        for run in runs {
            let run = run??;
            let runtime = run.runtime;
            let ms = (run.ended_at.unwrap_or(now_ms) - run.started_at).max(0);
            match usage.iter_mut().find(|u| u.runtime == runtime) {
                Some(u) => {
                    u.runs += 1;
                    u.total_ms += ms;
                }
                None => usage.push(RuntimeUsage {
                    runtime,
                    runs: 1,
                    total_ms: ms,
                }),
            }
        }
        usage.sort_by_key(|u| u.runtime != Runtime::ClaudeCode);
        Ok(usage)
    }

    /// Runs that are queued or working, plus those that ended at or after
    /// `ended_since` (Unix ms), in order.
    pub fn recent_runs(&self, ended_since: i64) -> Result<Vec<StoredRun>> {
        let mut stmt = self.conn.prepare(&format!(
            "{RUN_COLUMNS} WHERE status IN (?1, ?2) OR ended_at >= ?3 ORDER BY ord"
        ))?;
        let rows = stmt.query_map(
            params![
                status_text(&RunStatus::Queued)?,
                status_text(&RunStatus::Working)?,
                ended_since
            ],
            read_run,
        )?;
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
    ///
    /// Each ends at its last known activity: its latest message or
    /// approval, else its start. Events carry no time, so a run that only
    /// streamed events counts from its start to its last message. The time
    /// crewd was down never counts.
    pub fn mark_unfinished_runs_interrupted(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE runs SET status = ?1, ended_at = MAX(
                 started_at,
                 COALESCE((SELECT MAX(created_at) FROM messages WHERE run_id = runs.id), 0),
                 COALESCE((SELECT MAX(COALESCE(decided_at, created_at))
                           FROM approvals WHERE run_id = runs.id), 0)
             )
             WHERE status IN (?2, ?3)",
            params![
                status_text(&RunStatus::Interrupted)?,
                status_text(&RunStatus::Queued)?,
                status_text(&RunStatus::Working)?
            ],
        )?)
    }
}

/// Turns a unique name clash into [`StoreError::DuplicateAgentName`].
fn name_taken(e: rusqlite::Error, spec: &AgentSpec) -> StoreError {
    match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation
                && f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            StoreError::DuplicateAgentName(spec.name.clone())
        }
        e => e.into(),
    }
}

const AGENT_COLUMNS: &str = "SELECT id, spec, created_at, deleted_at FROM agents";

fn read_agent(r: &Row) -> rusqlite::Result<Result<StoredAgent>> {
    let id = AgentId(uuid_col(r, 0)?);
    let spec: String = r.get(1)?;
    let (created_at, deleted_at) = (r.get(2)?, r.get(3)?);
    Ok(serde_json::from_str(&spec)
        .map(|spec| StoredAgent {
            id,
            spec,
            created_at,
            deleted_at,
        })
        .map_err(StoreError::from))
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

const CHANNEL_COLUMNS: &str = "SELECT id, project_id, name, created_at, handoff_policy,
     handoff_budget, description FROM channels";

fn read_channel(r: &Row) -> rusqlite::Result<Result<StoredChannel>> {
    let channel = StoredChannel {
        id: ChannelId(uuid_col(r, 0)?),
        project: ProjectId(uuid_col(r, 1)?),
        name: r.get(2)?,
        created_at: r.get(3)?,
        handoff_policy: None,
        handoff_budget: r.get(5)?,
        description: r.get(6)?,
    };
    let policy: Option<String> = r.get(4)?;
    Ok(parse_policy(policy).map(|handoff_policy| StoredChannel {
        handoff_policy,
        ..channel
    }))
}

const THREAD_COLUMNS: &str = "SELECT id, channel_id, title, created_at, worktree_path,
     worktree_branch, worktree_base, handoff_policy, handoff_budget, handoffs_used,
     worktree_ready, archived_at FROM threads";

fn read_thread(r: &Row) -> rusqlite::Result<Result<StoredThread>> {
    let path: Option<String> = r.get(4)?;
    let branch: Option<String> = r.get(5)?;
    let base: Option<String> = r.get(6)?;
    let worktree = match (path, branch, base) {
        (Some(path), Some(branch), Some(base)) => Some(ThreadWorktree {
            path: PathBuf::from(path),
            branch,
            base,
            ready: r.get(10)?,
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
        archived_at: r.get(11)?,
    };
    let policy: Option<String> = r.get(7)?;
    Ok(parse_policy(policy).map(|handoff_policy| StoredThread {
        handoff_policy,
        ..thread
    }))
}

const RUN_COLUMNS: &str =
    "SELECT id, thread_id, agent_id, session_id, status, started_at, ended_at, runtime FROM runs";

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
        runtime: Runtime::ClaudeCode,
    };
    let runtime: String = r.get(7)?;
    Ok(parse_status(&status).and_then(|status| {
        Ok(StoredRun {
            status,
            runtime: parse_runtime(&runtime)?,
            ..run
        })
    }))
}
