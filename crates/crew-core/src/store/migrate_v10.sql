-- v9 to v10: a project knows what its folder holds and where agents
-- write, a thread's worktree may be a folder of repo worktrees, and a
-- queued run remembers the run it waits for.
ALTER TABLE projects ADD COLUMN kind TEXT NOT NULL DEFAULT '{"kind":"repo"}';
ALTER TABLE projects ADD COLUMN work_mode TEXT NOT NULL DEFAULT 'own_copy';
ALTER TABLE threads ADD COLUMN worktree_repos TEXT NOT NULL DEFAULT '[]';
ALTER TABLE runs ADD COLUMN waiting_for TEXT REFERENCES runs(id);
