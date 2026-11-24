-- v4 to v5: terminal sessions look runs up by session id.
CREATE INDEX runs_by_session ON runs(session_id);
