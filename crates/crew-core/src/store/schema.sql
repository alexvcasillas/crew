CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root TEXT NOT NULL,
    main_branch TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    setup_command TEXT,
    copy_files TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE channels (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    handoff_policy TEXT,
    handoff_budget INTEGER
);

CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    spec TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL REFERENCES channels(id),
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    worktree_path TEXT,
    worktree_branch TEXT,
    worktree_base TEXT,
    handoff_policy TEXT,
    handoff_budget INTEGER,
    handoffs_used INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    author_kind TEXT NOT NULL,
    author_agent TEXT REFERENCES agents(id),
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    ord INTEGER NOT NULL,
    run_id TEXT REFERENCES runs(id)
);
CREATE INDEX messages_by_thread ON messages(thread_id, ord);

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    agent_id TEXT NOT NULL REFERENCES agents(id),
    session_id TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    ord INTEGER NOT NULL
);
CREATE INDEX runs_by_thread ON runs(thread_id);
CREATE INDEX runs_by_status ON runs(status);

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
