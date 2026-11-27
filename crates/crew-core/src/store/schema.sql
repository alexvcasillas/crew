CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root TEXT NOT NULL,
    main_branch TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    setup_command TEXT,
    copy_files TEXT NOT NULL DEFAULT '[]',
    removed_at INTEGER,
    kind TEXT NOT NULL DEFAULT '{"kind":"repo"}',
    work_mode TEXT NOT NULL DEFAULT 'own_copy',
    pinned_at INTEGER
);

CREATE TABLE channels (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    handoff_policy TEXT,
    handoff_budget INTEGER,
    description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL COLLATE NOCASE,
    spec TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    deleted_at INTEGER
);
CREATE UNIQUE INDEX agents_by_live_name ON agents(name) WHERE deleted_at IS NULL;

CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL REFERENCES channels(id),
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    worktree_path TEXT,
    worktree_branch TEXT,
    worktree_base TEXT,
    worktree_ready INTEGER NOT NULL DEFAULT 0,
    handoff_policy TEXT,
    handoff_budget INTEGER,
    handoffs_used INTEGER NOT NULL DEFAULT 0,
    archived_at INTEGER,
    worktree_repos TEXT NOT NULL DEFAULT '[]',
    title_source TEXT NOT NULL DEFAULT 'first_line',
    first_title TEXT
);

CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    author_kind TEXT NOT NULL,
    author_agent TEXT REFERENCES agents(id),
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    ord INTEGER NOT NULL,
    run_id TEXT REFERENCES runs(id),
    approval_id TEXT REFERENCES approvals(id),
    quiet INTEGER NOT NULL DEFAULT 0,
    job_message INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX messages_by_thread ON messages(thread_id, ord);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    body TEXT NOT NULL,
    skill TEXT,
    model_override TEXT,
    agent_id TEXT NOT NULL REFERENCES agents(id),
    channel_id TEXT NOT NULL REFERENCES channels(id),
    thread_id TEXT NOT NULL REFERENCES threads(id),
    repeat_json TEXT NOT NULL,
    tell TEXT NOT NULL,
    on_ask TEXT NOT NULL,
    missed TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    paused_reason TEXT,
    next_due INTEGER,
    skipped INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX jobs_by_agent ON jobs(agent_id);
CREATE INDEX jobs_by_thread ON jobs(thread_id);

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    agent_id TEXT NOT NULL REFERENCES agents(id),
    session_id TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    ord INTEGER NOT NULL,
    runtime TEXT NOT NULL DEFAULT 'claude_code',
    handed_from TEXT REFERENCES runs(id),
    waiting_for TEXT REFERENCES runs(id),
    skill TEXT,
    model_override TEXT,
    message_id TEXT,
    job_id TEXT REFERENCES jobs(id),
    found INTEGER,
    summary TEXT,
    missed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX runs_by_thread ON runs(thread_id);
CREATE INDEX runs_by_status ON runs(status);
CREATE INDEX runs_by_session ON runs(session_id);

CREATE TABLE events (
    run_id TEXT NOT NULL REFERENCES runs(id),
    seq INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);

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

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE INDEX runs_ended ON runs(ended_at);
CREATE INDEX runs_by_job ON runs(job_id);
