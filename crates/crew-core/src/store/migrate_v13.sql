-- v12 to v13: a run keeps the message that asked for it and the model
-- it was asked to use for that message alone.
ALTER TABLE runs ADD COLUMN model_override TEXT;
ALTER TABLE runs ADD COLUMN message_id TEXT;
