-- v10 to v11: a project can be pinned to the top of the sidebar.
ALTER TABLE projects ADD COLUMN pinned_at INTEGER;
