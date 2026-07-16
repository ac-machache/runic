ALTER TABLE sessions ADD COLUMN IF NOT EXISTS agent TEXT;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS parent_session TEXT;

CREATE INDEX IF NOT EXISTS sessions_parent_idx
    ON sessions (tenant, parent_session, last_activity DESC, session_id DESC);
