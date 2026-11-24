-- v6 to v7: a small key/value table for the limits you set in the app, and
-- an index for listing ended runs.
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS runs_ended ON runs(ended_at);
