CREATE SCHEMA IF NOT EXISTS runic;

CREATE TABLE IF NOT EXISTS runic.sessions (
    tenant          TEXT        NOT NULL,
    session_id      TEXT        NOT NULL,
    last_seq        BIGINT      NOT NULL DEFAULT 0,
    event_count     BIGINT      NOT NULL DEFAULT 0,
    label           TEXT,
    agent           TEXT,
    parent_session  TEXT,
    run_count       BIGINT      NOT NULL DEFAULT 0,
    errored_runs    BIGINT      NOT NULL DEFAULT 0,
    input_tokens    BIGINT      NOT NULL DEFAULT 0,
    output_tokens   BIGINT      NOT NULL DEFAULT 0,
    last_run_status TEXT,
    last_run_at     TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_activity   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, session_id)
);

CREATE INDEX IF NOT EXISTS sessions_tenant_activity_idx
    ON runic.sessions (tenant, last_activity DESC);

CREATE INDEX IF NOT EXISTS sessions_parent_idx
    ON runic.sessions (tenant, parent_session, last_activity DESC, session_id DESC);

CREATE TABLE IF NOT EXISTS runic.events (
    tenant     TEXT        NOT NULL,
    session_id TEXT        NOT NULL,
    seq        BIGINT      NOT NULL,
    kind       TEXT        NOT NULL,
    run_id     TEXT,
    at         TIMESTAMPTZ NOT NULL,
    event      JSONB       NOT NULL,
    PRIMARY KEY (tenant, session_id, seq),
    FOREIGN KEY (tenant, session_id)
        REFERENCES runic.sessions (tenant, session_id) ON DELETE CASCADE
);
