CREATE TABLE IF NOT EXISTS runic.runs (
    run_id       TEXT        PRIMARY KEY,
    tenant       TEXT        NOT NULL,
    session_id   TEXT        NOT NULL,
    agent        TEXT        NOT NULL,
    status       TEXT        NOT NULL DEFAULT 'idle',
    error        TEXT,
    to_cancel    BOOLEAN     NOT NULL DEFAULT FALSE,
    steering     JSONB,
    input        JSONB,
    context      JSONB,
    attempt      INT         NOT NULL DEFAULT 0,
    max_attempts INT         NOT NULL DEFAULT 3,
    worker_id    TEXT,
    alive_at     TIMESTAMPTZ,
    execute_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant, session_id)
        REFERENCES runic.sessions (tenant, session_id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS runs_one_running_per_thread
    ON runic.runs (tenant, session_id) WHERE status = 'running';

CREATE INDEX IF NOT EXISTS runs_due_idx
    ON runic.runs (execute_at, created_at, run_id) WHERE status = 'idle';

CREATE INDEX IF NOT EXISTS runs_thread_queue_idx
    ON runic.runs (tenant, session_id, created_at, run_id) WHERE status = 'idle';

CREATE INDEX IF NOT EXISTS runs_liveness_idx
    ON runic.runs (alive_at) WHERE status = 'running';

CREATE INDEX IF NOT EXISTS runs_worker_idx
    ON runic.runs (worker_id) WHERE status = 'running';

CREATE INDEX IF NOT EXISTS runs_session_created_idx
    ON runic.runs (tenant, session_id, created_at DESC, run_id DESC);

ALTER TABLE runic.runs SET (
    fillfactor = 70,
    autovacuum_vacuum_scale_factor = 0.01,
    autovacuum_vacuum_threshold = 50,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_cost_delay = 0
);

CREATE OR REPLACE FUNCTION runic.runs_announce_claimable() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.status = 'idle' AND NEW.execute_at <= now() THEN
            PERFORM pg_notify('runic_claimable', '');
        END IF;
        RETURN NULL;
    END IF;

    IF NEW.status = 'idle' AND NEW.execute_at <= now()
       AND (OLD.status IS DISTINCT FROM 'idle'
            OR NEW.execute_at IS DISTINCT FROM OLD.execute_at) THEN
        PERFORM pg_notify('runic_claimable', '');
    ELSIF OLD.status = 'running' AND NEW.status IS DISTINCT FROM 'running'
          AND EXISTS (SELECT 1 FROM runic.runs sibling
                      WHERE sibling.tenant = NEW.tenant
                        AND sibling.session_id = NEW.session_id
                        AND sibling.status = 'idle'
                        AND sibling.execute_at <= now()) THEN
        PERFORM pg_notify('runic_claimable', '');
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS runs_announce_claimable ON runic.runs;

CREATE TRIGGER runs_announce_claimable
AFTER INSERT OR UPDATE ON runic.runs
FOR EACH ROW EXECUTE FUNCTION runic.runs_announce_claimable();
