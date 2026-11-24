-- v5 to v6: no run limit by default. Agents still on the old default of
-- 20 minutes, which could not be changed in the app, get no limit.
UPDATE agents
SET spec = json_set(spec, '$.max_run_minutes', 0)
WHERE json_extract(spec, '$.max_run_minutes') = 20;
