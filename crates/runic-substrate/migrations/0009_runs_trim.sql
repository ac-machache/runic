UPDATE runs SET status = 'idle'       WHERE status IN ('pending', 'queued');
UPDATE runs SET status = 'waiting'    WHERE status = 'paused';
UPDATE runs SET status = 'successful' WHERE status = 'success';
UPDATE runs SET status = 'failed'     WHERE status = 'error';
UPDATE runs SET status = 'failed', error = COALESCE(error, 'interrupted by deploy')
    WHERE status = 'running';

UPDATE sessions SET last_run_status = 'idle'
    WHERE last_run_status IN ('pending', 'queued');
UPDATE sessions SET last_run_status = 'waiting'    WHERE last_run_status = 'paused';
UPDATE sessions SET last_run_status = 'successful' WHERE last_run_status = 'success';
UPDATE sessions SET last_run_status = 'failed'
    WHERE last_run_status IN ('error', 'running');

ALTER TABLE runs ALTER COLUMN status SET DEFAULT 'idle';

ALTER TABLE runs RENAME COLUMN cancel_requested TO to_cancel;

ALTER TABLE runs ADD COLUMN IF NOT EXISTS started_at  TIMESTAMPTZ;
ALTER TABLE runs ADD COLUMN IF NOT EXISTS finished_at TIMESTAMPTZ;

ALTER TABLE runs DROP COLUMN IF EXISTS claimed_by;
ALTER TABLE runs DROP COLUMN IF EXISTS lease_expires_at;
ALTER TABLE runs DROP COLUMN IF EXISTS input;
ALTER TABLE runs DROP COLUMN IF EXISTS context;

DELETE FROM runs AS r
    WHERE NOT EXISTS (
        SELECT 1 FROM sessions AS s
        WHERE s.tenant = r.tenant AND s.session_id = r.session_id
    );

ALTER TABLE runs ADD CONSTRAINT runs_session_fk
    FOREIGN KEY (tenant, session_id)
    REFERENCES sessions (tenant, session_id) ON DELETE CASCADE;

DROP INDEX IF EXISTS runs_status_idx;
DROP INDEX IF EXISTS runs_thread_idx;

CREATE UNIQUE INDEX IF NOT EXISTS runs_one_running_per_thread
    ON runs (tenant, session_id) WHERE status = 'running';

DROP TABLE IF EXISTS thread_leases;
