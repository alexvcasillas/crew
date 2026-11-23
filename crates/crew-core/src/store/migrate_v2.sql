-- v1 to v2: worktrees, handoff settings, message runs and approvals.
ALTER TABLE projects ADD COLUMN setup_command TEXT;
ALTER TABLE projects ADD COLUMN copy_files TEXT NOT NULL DEFAULT '[]';

ALTER TABLE channels ADD COLUMN handoff_policy TEXT;
ALTER TABLE channels ADD COLUMN handoff_budget INTEGER;

ALTER TABLE threads ADD COLUMN worktree_path TEXT;
ALTER TABLE threads ADD COLUMN worktree_branch TEXT;
ALTER TABLE threads ADD COLUMN worktree_base TEXT;
ALTER TABLE threads ADD COLUMN handoff_policy TEXT;
ALTER TABLE threads ADD COLUMN handoff_budget INTEGER;
ALTER TABLE threads ADD COLUMN handoffs_used INTEGER NOT NULL DEFAULT 0;

ALTER TABLE messages ADD COLUMN run_id TEXT REFERENCES runs(id);

CREATE TABLE approvals (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    run_id TEXT NOT NULL REFERENCES runs(id),
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    decided_at INTEGER
);
CREATE INDEX approvals_by_status ON approvals(status, thread_id);
