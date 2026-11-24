-- v3 to v4: runs record which CLI ran them. Before v4 only Claude Code did.
ALTER TABLE runs ADD COLUMN runtime TEXT NOT NULL DEFAULT 'claude_code';
