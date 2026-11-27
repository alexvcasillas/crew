-- v14 to v15: scheduled jobs, their runs, and replies that may be pruned.
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
ALTER TABLE runs ADD COLUMN job_id TEXT REFERENCES jobs(id);
ALTER TABLE runs ADD COLUMN found INTEGER;
ALTER TABLE runs ADD COLUMN summary TEXT;
ALTER TABLE runs ADD COLUMN missed INTEGER NOT NULL DEFAULT 0;
CREATE INDEX runs_by_job ON runs(job_id);
ALTER TABLE messages ADD COLUMN quiet INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN job_message INTEGER NOT NULL DEFAULT 0;
