-- v8 to v9: a run started by a handoff remembers the run that asked for
-- it, and a removed project is kept with the time it was removed.
ALTER TABLE runs ADD COLUMN handed_from TEXT REFERENCES runs(id);
ALTER TABLE projects ADD COLUMN removed_at INTEGER;
