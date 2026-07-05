-- One row per run: the operational registry (status, claims, leases).
-- The event log stays the source of truth for CONTENT; this table answers
-- "what runs exist and who is executing them" without folding events.
CREATE TABLE IF NOT EXISTS runs (
    run_id           TEXT        PRIMARY KEY,
    tenant           TEXT        NOT NULL,
    session_id       TEXT        NOT NULL,
    agent            TEXT        NOT NULL,
    status           TEXT        NOT NULL DEFAULT 'pending',
    error            TEXT,
    claimed_by       TEXT,
    lease_expires_at TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS runs_thread_idx
    ON runs (tenant, session_id, created_at DESC);

CREATE INDEX IF NOT EXISTS runs_status_idx
    ON runs (status, lease_expires_at);
