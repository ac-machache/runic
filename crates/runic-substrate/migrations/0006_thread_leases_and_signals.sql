CREATE TABLE IF NOT EXISTS thread_leases (
    tenant TEXT NOT NULL,
    session_id TEXT NOT NULL,
    claimed_by TEXT NOT NULL,
    lease_expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant, session_id)
);

ALTER TABLE runs ADD COLUMN IF NOT EXISTS cancel_requested BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE runs ADD COLUMN IF NOT EXISTS steering JSONB;
