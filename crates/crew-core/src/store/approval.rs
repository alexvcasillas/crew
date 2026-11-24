//! Requests that wait for you: handoffs between agents and tool permissions.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{Result, Store, StoreError, enum_text, now_ms, parse_enum, uuid_col};
use crate::ids::{AgentId, ApprovalId, RunId, ThreadId};

/// What is being asked. On the wire:
/// `{"kind":"handoff","from":..,"to":..,"brief":..,"paused":..}` or
/// `{"kind":"permission","tool":..,"input":..,"tool_use_id":..}`.
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
        /// Claude's id for the tool call, to match it in the run's events.
        #[serde(default)]
        tool_use_id: Option<String>,
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

/// Longest tool input shown in a permission ask.
const SHORT_INPUT: usize = 80;

/// A short, one-line view of a tool's input: its label, command or path
/// if it has one, else its JSON.
pub fn short_input(input: &serde_json::Value) -> String {
    let text = ["label", "command", "file_path", "path", "url", "pattern"]
        .iter()
        .find_map(|k| input.get(k).and_then(serde_json::Value::as_str))
        .map(String::from)
        .unwrap_or_else(|| match input {
            serde_json::Value::Null => String::new(),
            serde_json::Value::Object(map) if map.is_empty() => String::new(),
            other => other.to_string(),
        });
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let cut: String = first.chars().take(SHORT_INPUT).collect();
    if cut.len() < first.len() || lines.next().is_some() {
        format!("{cut}...")
    } else {
        cut
    }
}

/// The thread note for a permission ask of `agent`.
/// MCP tools, named `mcp__<server>__<tool>`, read as `server/tool`. A
/// Codex `Permissions` ask reads as wanting more access.
pub fn permission_text(agent: &str, tool: &str, input: &serde_json::Value) -> String {
    let shown = short_input(input);
    if tool == "Permissions" {
        return format!("{agent} wants more access: {shown}");
    }
    let (verb, what) = match tool.strip_prefix("mcp__") {
        Some(rest) => ("use", rest.replacen("__", "/", 1)),
        None => ("run", tool.to_string()),
    };
    if shown.is_empty() {
        format!("{agent} wants to {verb} {what}.")
    } else {
        format!("{agent} wants to {verb} {what}: {shown}")
    }
}

/// The thread note for a handoff ask.
pub fn handoff_ask_text(from: &str, to: &str) -> String {
    format!("{from} wants to bring in {to}.")
}

/// The thread note for a handoff held back by a used-up budget.
pub fn paused_text(budget: u32) -> String {
    format!("Paused. This thread used its {budget} handoffs.")
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
            tool_use_id: None,
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

    /// Every approval of a thread, whatever its status, oldest first.
    pub fn approvals(&self, thread: ThreadId) -> Result<Vec<StoredApproval>> {
        let mut stmt = self.conn.prepare(&format!(
            "{APPROVAL_COLUMNS} WHERE thread_id = ?1 ORDER BY created_at, rowid"
        ))?;
        let rows = stmt.query_map([thread.to_string()], read_approval)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn short_input_picks_the_command_and_cuts_it() {
        assert_eq!(short_input(&json!({"command": "ls -la"})), "ls -la");
        // A shorter label wins over the full command.
        assert_eq!(
            short_input(&json!({"command": "/bin/zsh -lc 'ls'", "label": "ls"})),
            "ls"
        );
        assert_eq!(
            short_input(&json!({"file_path": "/work/demo/a"})),
            "/work/demo/a"
        );
        assert_eq!(short_input(&json!({})), "");
        assert_eq!(short_input(&json!({"n": 1})), r#"{"n":1}"#);
        assert_eq!(short_input(&json!({"command": "a\nb"})), "a...");
        let long = "x".repeat(100);
        assert_eq!(
            short_input(&json!({ "command": long })),
            format!("{}...", "x".repeat(80))
        );
    }

    #[test]
    fn mcp_tools_read_as_server_and_tool() {
        assert_eq!(
            permission_text("Ada", "Bash", &json!({"command": "ls"})),
            "Ada wants to run Bash: ls"
        );
        assert_eq!(
            permission_text("Ada", "mcp__github__create_issue", &json!({})),
            "Ada wants to use github/create_issue."
        );
        assert_eq!(
            permission_text("Ada", "mcp__github__create_issue", &json!({"title": "x"})),
            r#"Ada wants to use github/create_issue: {"title":"x"}"#
        );
        assert_eq!(
            permission_text("Ada", "mcp__github", &json!({})),
            "Ada wants to use github."
        );
        assert_eq!(
            permission_text(
                "Ada",
                "Permissions",
                &json!({"label": "write gen, network"})
            ),
            "Ada wants more access: write gen, network"
        );
    }
}
