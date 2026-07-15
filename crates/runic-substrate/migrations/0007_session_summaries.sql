ALTER TABLE sessions ADD COLUMN IF NOT EXISTS run_count bigint NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS errored_runs bigint NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS input_tokens bigint NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS output_tokens bigint NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS last_run_status text;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS last_run_at timestamptz;
CREATE INDEX IF NOT EXISTS runs_session_created_idx
    ON runs (tenant, session_id, created_at DESC, run_id DESC);
