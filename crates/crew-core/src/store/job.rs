//! Scheduled jobs: what a job is, and how the store keeps jobs and the
//! runs they start.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{
    RUN_COLUMNS, Result, RunStatus, Store, StoreError, StoredRun, enum_text, now_ms, parse_enum,
    read_run, status_text, uuid_col,
};
use crate::ids::{AgentId, ChannelId, JobId, RunId, ThreadId};
use crate::schedule::Repeat;

/// How much of a run's reply stands in for a missing summary.
const REPLY_START_CHARS: usize = 300;

/// When a job tells you about a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tell {
    /// Only runs that found something.
    Changes,
    EveryRun,
}

/// What a job's run does when its agent asks for permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnAsk {
    /// Wait for you to answer.
    Wait,
    /// Refuse and carry on.
    Deny,
}

/// What a job does with slots that came due while the Mac was asleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Missed {
    /// Run once when it wakes.
    OnceOnWake,
    Skip,
}

/// A scheduled job as saved in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredJob {
    pub id: JobId,
    pub name: String,
    pub body: String,
    pub skill: Option<String>,
    pub model_override: Option<String>,
    pub agent: AgentId,
    pub channel: ChannelId,
    /// The thread its messages and runs live in.
    pub thread: ThreadId,
    pub repeat: Repeat,
    pub tell: Tell,
    pub on_ask: OnAsk,
    pub missed: Missed,
    pub enabled: bool,
    /// Why crewd turned the job off by itself, if it did.
    pub paused_reason: Option<String>,
    /// Unix milliseconds of the next run. `None` when the job is off.
    pub next_due: Option<i64>,
    /// Slots skipped since you last looked.
    pub skipped: u32,
    /// Unix milliseconds.
    pub created_at: i64,
}

/// What it takes to create a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewJob {
    pub name: String,
    pub body: String,
    pub skill: Option<String>,
    pub model_override: Option<String>,
    pub agent: AgentId,
    pub channel: ChannelId,
    pub repeat: Repeat,
    pub tell: Tell,
    pub on_ask: OnAsk,
    pub missed: Missed,
    pub enabled: bool,
}

/// The fields of a job that can change. `None` leaves a field alone; for
/// `skill` and `model_override`, `Some(None)` clears it. The channel is
/// not editable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub skill: Option<Option<String>>,
    #[serde(default)]
    pub model_override: Option<Option<String>>,
    #[serde(default)]
    pub agent: Option<AgentId>,
    #[serde(default)]
    pub repeat: Option<Repeat>,
    #[serde(default)]
    pub tell: Option<Tell>,
    #[serde(default)]
    pub on_ask: Option<OnAsk>,
    #[serde(default)]
    pub missed: Option<Missed>,
}

const JOB_COLUMNS: &str = "SELECT id, name, body, skill, model_override, agent_id, channel_id,
     thread_id, repeat_json, tell, on_ask, missed, enabled, paused_reason, next_due, skipped,
     created_at FROM jobs";

fn read_job(r: &Row) -> rusqlite::Result<Result<StoredJob>> {
    let id = JobId(uuid_col(r, 0)?);
    let agent = AgentId(uuid_col(r, 5)?);
    let channel = ChannelId(uuid_col(r, 6)?);
    let thread = ThreadId(uuid_col(r, 7)?);
    let (repeat, tell, on_ask, missed): (String, String, String, String) =
        (r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?);
    let (name, body, skill, model_override) = (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
    let (enabled, paused_reason, next_due) = (r.get(12)?, r.get(13)?, r.get(14)?);
    let (skipped, created_at) = (r.get(15)?, r.get(16)?);
    Ok((|| {
        Ok(StoredJob {
            id,
            name,
            body,
            skill,
            model_override,
            agent,
            channel,
            thread,
            repeat: serde_json::from_str(&repeat)?,
            tell: parse_enum(&tell)?,
            on_ask: parse_enum(&on_ask)?,
            missed: parse_enum(&missed)?,
            enabled,
            paused_reason,
            next_due,
            skipped,
            created_at,
        })
    })())
}

impl Store {
    /// Creates a job, its thread (titled with the job's name) and the
    /// first message, which holds the instructions. `next_due` starts
    /// unset: crewd works it out.
    pub fn create_job(&self, new: &NewJob) -> Result<JobId> {
        self.in_transaction(|s| {
            let thread = s.create_thread_titled(new.channel, &new.name, true)?;
            s.add_job_message(thread, &new.body)?;
            let id = JobId::new();
            s.conn.execute(
                "INSERT INTO jobs (id, name, body, skill, model_override, agent_id, channel_id,
                                   thread_id, repeat_json, tell, on_ask, missed, enabled,
                                   created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    id.to_string(),
                    new.name,
                    new.body,
                    new.skill,
                    new.model_override,
                    new.agent.to_string(),
                    new.channel.to_string(),
                    thread.to_string(),
                    serde_json::to_string(&new.repeat)?,
                    enum_text(&new.tell)?,
                    enum_text(&new.on_ask)?,
                    enum_text(&new.missed)?,
                    new.enabled,
                    now_ms(),
                ],
            )?;
            Ok(id)
        })
    }

    pub fn job(&self, id: JobId) -> Result<StoredJob> {
        let row = self
            .conn
            .query_row(
                &format!("{JOB_COLUMNS} WHERE id = ?1"),
                [id.to_string()],
                read_job,
            )
            .optional()?;
        row.ok_or(StoreError::NotFound("job"))?
    }

    /// Every job, oldest first.
    pub fn jobs(&self) -> Result<Vec<StoredJob>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{JOB_COLUMNS} ORDER BY created_at, rowid"))?;
        let rows = stmt.query_map([], read_job)?;
        rows.map(|r| r?).collect()
    }

    /// The jobs an agent runs, oldest first.
    pub fn jobs_for_agent(&self, agent: AgentId) -> Result<Vec<StoredJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "{JOB_COLUMNS} WHERE agent_id = ?1 ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map([agent.to_string()], read_job)?;
        rows.map(|r| r?).collect()
    }

    /// The job whose thread this is, if any.
    pub fn job_for_thread(&self, thread: ThreadId) -> Result<Option<StoredJob>> {
        self.conn
            .query_row(
                &format!("{JOB_COLUMNS} WHERE thread_id = ?1"),
                [thread.to_string()],
                read_job,
            )
            .optional()?
            .transpose()
    }

    /// Applies the fields set in `patch`.
    pub fn update_job(&self, id: JobId, patch: &JobPatch) -> Result<()> {
        let job = self.job(id)?;
        self.conn.execute(
            "UPDATE jobs SET name = ?2, body = ?3, skill = ?4, model_override = ?5,
                 agent_id = ?6, repeat_json = ?7, tell = ?8, on_ask = ?9, missed = ?10
             WHERE id = ?1",
            params![
                id.to_string(),
                patch.name.as_ref().unwrap_or(&job.name),
                patch.body.as_ref().unwrap_or(&job.body),
                patch.skill.as_ref().unwrap_or(&job.skill),
                patch.model_override.as_ref().unwrap_or(&job.model_override),
                patch.agent.unwrap_or(job.agent).to_string(),
                serde_json::to_string(&patch.repeat.unwrap_or(job.repeat))?,
                enum_text(&patch.tell.unwrap_or(job.tell))?,
                enum_text(&patch.on_ask.unwrap_or(job.on_ask))?,
                enum_text(&patch.missed.unwrap_or(job.missed))?,
            ],
        )?;
        Ok(())
    }

    /// Deletes a job. Its thread, messages and runs stay; the runs no
    /// longer point at a job.
    pub fn delete_job(&self, id: JobId) -> Result<()> {
        self.in_transaction(|s| {
            s.conn.execute(
                "UPDATE runs SET job_id = NULL WHERE job_id = ?1",
                [id.to_string()],
            )?;
            let gone = s
                .conn
                .execute("DELETE FROM jobs WHERE id = ?1", [id.to_string()])?;
            if gone == 0 {
                return Err(StoreError::NotFound("job"));
            }
            Ok(())
        })
    }

    pub fn set_job_next_due(&self, id: JobId, next_due: Option<i64>) -> Result<()> {
        self.set_job_column(id, "next_due", next_due)
    }

    pub fn set_job_skipped(&self, id: JobId, skipped: u32) -> Result<()> {
        self.set_job_column(id, "skipped", skipped)
    }

    /// Sets or clears why the job was turned off.
    pub fn set_job_paused(&self, id: JobId, reason: Option<&str>) -> Result<()> {
        self.set_job_column(id, "paused_reason", reason)
    }

    fn set_job_column(&self, id: JobId, column: &str, value: impl rusqlite::ToSql) -> Result<()> {
        let changed = self.conn.execute(
            &format!("UPDATE jobs SET {column} = ?2 WHERE id = ?1"),
            params![id.to_string(), value],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("job"));
        }
        Ok(())
    }

    /// Adds a run of a job in the `Queued` state. `missed` is how many
    /// due slots it stands for.
    pub fn create_job_run(
        &self,
        thread: ThreadId,
        agent: AgentId,
        job: JobId,
        missed: u32,
    ) -> Result<RunId> {
        let id = RunId::new();
        self.conn.execute(
            "INSERT INTO runs (id, thread_id, agent_id, session_id, status, started_at, ord,
                               job_id, missed)
             VALUES (?1, ?2, ?3, '', ?4, ?5, (SELECT COALESCE(MAX(ord) + 1, 0) FROM runs),
                     ?6, ?7)",
            params![
                id.to_string(),
                thread.to_string(),
                agent.to_string(),
                status_text(&RunStatus::Queued)?,
                now_ms(),
                job.to_string(),
                missed
            ],
        )?;
        Ok(id)
    }

    /// Saves what a job's run reported: whether it found anything, and a
    /// line about it.
    pub fn set_run_report(&self, run: RunId, found: bool, summary: Option<&str>) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE runs SET found = ?2, summary = ?3 WHERE id = ?1",
            params![run.to_string(), found, summary],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("run"));
        }
        Ok(())
    }

    /// The job's newest `n` runs, newest first.
    pub fn job_runs(&self, job: JobId, n: u32) -> Result<Vec<StoredRun>> {
        let mut stmt = self.conn.prepare(&format!(
            "{RUN_COLUMNS} WHERE job_id = ?1 ORDER BY ord DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![job.to_string(), n], read_run)?;
        rows.map(|r| r?).collect()
    }

    /// What the job's newest `n` runs that found something said, newest
    /// first: when the run ended (when it started, if it never ended) in
    /// Unix ms, and its summary, else the start of its reply.
    pub fn recent_found(&self, job: JobId, n: u32) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(ended_at, started_at), summary,
                    (SELECT body FROM messages
                     WHERE run_id = runs.id AND author_kind = 'agent'
                     ORDER BY ord DESC LIMIT 1)
             FROM runs WHERE job_id = ?1 AND found = 1 ORDER BY ord DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![job.to_string(), n], |r| {
            let at: i64 = r.get(0)?;
            let summary: Option<String> = r.get(1)?;
            let reply: Option<String> = r.get(2)?;
            let text = summary.unwrap_or_else(|| {
                reply
                    .unwrap_or_default()
                    .chars()
                    .take(REPLY_START_CHARS)
                    .collect()
            });
            Ok((at, text))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
