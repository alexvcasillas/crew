-- v13 to v14: where a thread's title came from, and the first line it had
-- before a generated title replaced it.
ALTER TABLE threads ADD COLUMN title_source TEXT NOT NULL DEFAULT 'first_line';
ALTER TABLE threads ADD COLUMN first_title TEXT;
