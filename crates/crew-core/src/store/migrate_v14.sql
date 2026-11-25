-- v13 to v14: where a thread's title came from, and the first line it had
-- before a generated title replaced it.
ALTER TABLE threads ADD COLUMN title_source TEXT NOT NULL DEFAULT 'first_line';
ALTER TABLE threads ADD COLUMN first_title TEXT;
-- A project's own skill is shown or hidden per project: an old setting
-- for every project becomes one for each of them.
INSERT OR IGNORE INTO settings (key, value)
SELECT substr(s.key, 1, instr(s.key, ':project:') + 8) || p.id || ':'
       || substr(s.key, instr(s.key, ':project:') + 9),
       s.value
FROM settings s, projects p
WHERE s.key LIKE 'skill_shown:%:project:%';
DELETE FROM settings
WHERE key LIKE 'skill_shown:%:project:%'
  AND key NOT IN (
      SELECT s.key FROM settings s, projects p
      WHERE s.key LIKE 'skill_shown:%:project:' || p.id || ':%'
  );
