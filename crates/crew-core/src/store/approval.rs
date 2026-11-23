//! Requests that wait for you: handoffs between agents and tool permissions.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{Result, Store, StoreError, enum_text, now_ms, parse_enum, uuid_col};
use crate::ids::{AgentId, ApprovalId, RunId, ThreadId};

/// What is being asked. On the wire:
/// `{"kind":"handoff","from":..,"to":..,"brief":..,"paused":..}` or
/// `{"kind":"permission","tool":..,"input":..}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalKind {
    Handoff {
        from: AgentId,
        to: AgentId,
        brief: String,
        /// Asked because the thread's budget was used up. Approving it
        /// raises the budget by one, so just this handoff runs.
        #[serde(default)]
        paused: bool,
    },
    Permission {
        tool: String,
        input: serde_json::Value,
    },
}

impl ApprovalKind {
    fn name(&self) -> &'static str {
        match self {
            ApprovalKind::Handoff { .. } => "handoff",
            ApprovalKind::Permission { .. } => "permission",
        }
    }
}

/// Where an approval stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Declined,
    /// The run that asked ended before anyone decided.
    Cancelled,
}

/// An approval as saved in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredApproval {
    pub id: ApprovalId,
    pub thread: ThreadId,
    /// The run that asked.
    pub run: RunId,
    pub kind: ApprovalKind,
    pub status: ApprovalStatus,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds, set once the status leaves `Pending`.
    pub decided_at: Option<i64>,
}

const APPROVAL_COLUMNS: &str =
    "SELECT id, thread_id, run_id, payload_json, status, created_at, decided_at FROM approvals";

fn read_approval(r: &Row) -> rusqlite::Result<Result<StoredApproval>> {
    let payload: String = r.get(3)?;
    let status: String = r.get(4)?;
    let approval = StoredApproval {
        id: ApprovalId(uuid_col(r, 0)?),
        thread: ThreadId(uuid_col(r, 1)?),
        run: RunId(uuid_col(r, 2)?),
        kind: ApprovalKind::Permission {
            tool: String::new(),
            input: serde_json::Value::Null,
        },
        status: ApprovalStatus::Pending,
        created_at: r.get(5)?,
        decided_at: r.get(6)?,
    };
    let kind = match serde_json::from_str(&payload) {
        Ok(kind) => kind,
        Err(e) => return Ok(Err(e.into())),
    };
    Ok(parse_enum(&status).map(|status| StoredApproval {
        kind,
        status,
        ..approval
    }))
}

impl Store {
    /// Saves a `Pending` approval asked by run `run`.
    pub fn create_approval(
        &self,
        t: ThreadId,
        run: RunId,
        kind: &ApprovalKind,
    ) -> Result<ApprovalId> {
        let id = ApprovalId::new();
        self.conn.execute(
            "INSERT INTO approvals (id, thread_id, run_id, kind, payload_json, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id.to_string(),
                t.to_string(),
                run.to_string(),
                kind.name(),
                serde_json::to_string(kind)?,
                enum_text(&ApprovalStatus::Pending)?,
                now_ms()
            ],
        )?;
        Ok(id)
    }

    pub fn approval(&self, id: ApprovalId) -> Result<StoredApproval> {
        let row = self
            .conn
            .query_row(
                &format!("{APPROVAL_COLUMNS} WHERE id = ?1"),
                [id.to_string()],
                read_approval,
            )
            .optional()?;
        row.ok_or(StoreError::NotFound("approval"))?
    }

    /// Decides a pending approval. `edited_brief` replaces a handoff's brief
    /// and is ignored for permissions. Fails with `NotFound` if the approval
    /// does not exist or is no longer pending.
    pub fn decide_approval(
        &self,
        id: ApprovalId,
        status: ApprovalStatus,
        edited_brief: Option<String>,
    ) -> Result<StoredApproval> {
        let mut approval = self.approval(id)?;
        if approval.status != ApprovalStatus::Pending {
            return Err(StoreError::NotFound("pending approval"));
        }
        if let (ApprovalKind::Handoff { brief, .. }, Some(edited)) =
            (&mut approval.kind, edited_brief)
        {
            *brief = edited;
        }
        let changed = self.conn.execute(
            "UPDATE approvals SET status = ?2, payload_json = ?3, decided_at = ?4
             WHERE id = ?1 AND status = ?5",
            params![
                id.to_string(),
                enum_text(&status)?,
                serde_json::to_string(&approval.kind)?,
                now_ms(),
                enum_text(&ApprovalStatus::Pending)?
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("pending approval"));
        }
        self.approval(id)
    }

    /// Pending approvals, oldest first, for one thread or all of them.
    pub fn pending_approvals(&self, thread: Option<ThreadId>) -> Result<Vec<StoredApproval>> {
        let mut stmt = self.conn.prepare(&format!(
            "{APPROVAL_COLUMNS} WHERE status = ?1 AND (?2 IS NULL OR thread_id = ?2)
             ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map(
            params![
                enum_text(&ApprovalStatus::Pending)?,
                thread.map(|t| t.to_string())
            ],
            read_approval,
        )?;
        rows.map(|r| r?).collect()
    }

    /// Marks the run's pending approvals `Cancelled`. Returns how many changed.
    pub fn cancel_approvals_for_run(&self, run: RunId) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE approvals SET status = ?2, decided_at = ?3 WHERE run_id = ?1 AND status = ?4",
            params![
                run.to_string(),
                enum_text(&ApprovalStatus::Cancelled)?,
                now_ms(),
                enum_text(&ApprovalStatus::Pending)?
            ],
        )?)
    }
}
