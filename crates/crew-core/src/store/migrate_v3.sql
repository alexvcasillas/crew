-- v2 to v3: agents can be deleted, which frees their name.
CREATE TABLE agents_v3 (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL COLLATE NOCASE,
    spec TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    deleted_at INTEGER
);
INSERT INTO agents_v3 (id, name, spec, created_at) SELECT id, name, spec, created_at FROM agents;
DROP TABLE agents;
ALTER TABLE agents_v3 RENAME TO agents;
CREATE UNIQUE INDEX agents_by_live_name ON agents(name) WHERE deleted_at IS NULL;
