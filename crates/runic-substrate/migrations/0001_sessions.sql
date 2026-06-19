-- runic-sessions: the event-sourced session store.

-- One row per conversation: the monotonic seq counter + derived metadata.
-- Updated on every append; also the anchor for listing and stale-GC.
CREATE TABLE IF NOT EXISTS sessions (
    tenant        TEXT        NOT NULL,
    session_id    TEXT        NOT NULL,
    last_seq      BIGINT      NOT NULL DEFAULT 0,
    event_count   BIGINT      NOT NULL DEFAULT 0,
    label         TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_activity TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, session_id)
);

CREATE INDEX IF NOT EXISTS sessions_tenant_activity_idx
    ON sessions (tenant, last_activity DESC);

-- The append-only log. Source of truth; AgentState is folded from this.
CREATE TABLE IF NOT EXISTS session_events (
    tenant      TEXT        NOT NULL,
    session_id  TEXT        NOT NULL,
    seq         BIGINT      NOT NULL,
    kind        TEXT        NOT NULL,
    run_id      TEXT,
    at          TIMESTAMPTZ NOT NULL,
    event       JSONB       NOT NULL,
    PRIMARY KEY (tenant, session_id, seq),
    FOREIGN KEY (tenant, session_id)
        REFERENCES sessions (tenant, session_id) ON DELETE CASCADE
);
