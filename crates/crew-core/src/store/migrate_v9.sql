-- v8 to v9: a run started by a handoff remembers the run that asked for it.
ALTER TABLE runs ADD COLUMN handed_from TEXT REFERENCES runs(id);
