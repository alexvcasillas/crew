-- v7 to v8: threads can be archived and channels get a description.
ALTER TABLE threads ADD COLUMN archived_at INTEGER;
ALTER TABLE channels ADD COLUMN description TEXT NOT NULL DEFAULT '';
